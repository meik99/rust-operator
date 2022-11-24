use anyhow::{Result};
use futures::StreamExt;
use k8s_openapi::{
    api::{
        apps::v1::{DaemonSet, DaemonSetSpec},
        core::v1::{
            Container, EnvVar, HostPathVolumeSource, PodSpec, SecurityContext, Volume, VolumeMount,
        },
    },
    apimachinery::pkg::apis::meta::v1::LabelSelector,
};
use kube::{
    api::{Api, ListParams, Patch, PatchParams, PostParams},
    core::{ObjectMeta},
    runtime::controller::{Action, Controller},
    Client, CustomResource, CustomResourceExt,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs::File,
    io::{BufRead, Write},
    sync::Arc,
};
use thiserror::Error;
use tokio::time::Duration;
use tracing::*;

#[derive(CustomResource, Debug, Serialize, Deserialize, Default, Clone, JsonSchema)]
#[kube(group = "dynatrace.com", version = "v1", kind = "DynaKube", namespaced)]
#[serde(rename_all = "camelCase")]
pub struct DynaKubeSpec {
    api_url: String,
    token: String,
}

#[derive(Debug, Error)]
enum Error {
    #[error("Failed to create DaemonSet: {0}")]
    DaemonSetCreationFailed(#[source] kube::Error),
    #[error("MissingObjectKey: {0}")]
    MissingObjectKey(&'static str),
}

struct Data {
    client: Client,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let client = Client::try_default().await?;
    let api: Api<DynaKube> = Api::all(client.clone());
    let daemonset_api: Api<DaemonSet> = Api::all(client.clone());

    let (mut reload_tx, reload_rx) = futures::channel::mpsc::channel(0);
    // Using a regular background thread since tokio::io::stdin() doesn't allow aborting reads,
    // and its worker prevents the Tokio runtime from shutting down.
    std::thread::spawn(move || {
        for _ in std::io::BufReader::new(std::io::stdin()).lines() {
            let _ = reload_tx.try_send(());
        }
    });

    let mut crd_file = File::create("deployment/crd.yaml")?;
    crd_file.write_all(serde_yaml::to_string(&DynaKube::crd()).unwrap().as_bytes())?;

    Controller::new(api, ListParams::default())
        .owns(daemonset_api, ListParams::default())
        .reconcile_all_on(reload_rx.map(|_| ()))
        .shutdown_on_signal()
        .run(reconcile, error_policy, Arc::new(Data { client }))
        .for_each(|res| async move {
            match res {
                Ok(o) => info!("reconciled {:?}", o),
                Err(e) => warn!("reconcile failed: {}", e),
            }
        })
        .await;

    Ok(())
}

async fn reconcile(generator: Arc<DynaKube>, ctx: Arc<Data>) -> Result<Action, Error> {
    println!("starting reconciliation");
    let client = &ctx.client;
    
    let name = match generator.metadata.name.clone() {
        Some(metadata_name) => metadata_name,
        None => return Err(Error::MissingObjectKey("name")),
    };

    let namespace = match generator.metadata.namespace.clone() {
        Some(metadata_namespace) => metadata_namespace,
        None => return Err(Error::MissingObjectKey("namespace")),
    };

    let daemonset_api: Api<DaemonSet> = Api::namespaced(client.clone(), &namespace);

    println!("reconciling {} in {}", name, namespace);

    let desired_daemonset = build_daemonset(
        &name,
        &namespace,
        &generator.spec.api_url,
        &generator.spec.token,
    );
    let name = desired_daemonset.metadata.name.clone().unwrap();
    let result = daemonset_api.get(name.as_str()).await;
    let mut exists = false;

    match result {
        Ok(_) => {
            println!("daemonset exists");
            exists = true;
        }
        Err(err) => {
            println!("{}", err);
            match &err {
                kube::Error::Api(api_err) => if api_err.code != 404 {
                    return Err(Error::DaemonSetCreationFailed(err))
                },
                _ => return Err(Error::DaemonSetCreationFailed(err)),
            };
        }
    };

    // println!("{}", json!(desired_daemonset));

    let result = if exists {
        println!("patching daemonset");
        daemonset_api
            .patch(
                &name,
                &PatchParams::apply("dynakube.kube-rt.dynatrace.com"),
                &Patch::Apply(&desired_daemonset),
            )
            .await
    } else {
        println!("creating daemonset");
        daemonset_api
            .create(&PostParams::default(), &desired_daemonset)
            .await
    };

    match result {
        Ok(_) => return Ok(Action::requeue(Duration::from_secs(300))),
        Err(err) => {
            println!("{}", err);
            return Err(Error::DaemonSetCreationFailed(err))
        }
    }
}

fn error_policy(_object: Arc<DynaKube>, _error: &Error, _ctx: Arc<Data>) -> Action {
    Action::requeue(Duration::from_secs(10))
}

fn build_daemonset(
    name: &String,
    namespace: &String,
    api_url: &String,
    token: &String,
) -> DaemonSet {
    let mut meta = ObjectMeta::default();

    meta.name = Some(format!("{}-daemonset", name));
    meta.namespace = Some(namespace.to_string());

    let mut selector = BTreeMap::new();

    selector.insert(String::from("name"), String::from("dynatrace-oneagent"));

    let mut template_meta = ObjectMeta::default();

    template_meta.labels = Some(selector.clone());

    let mut spec = DaemonSetSpec::default();

    spec.selector = LabelSelector {
        match_labels: Some(selector.clone()),
        match_expressions: None,
    };

    spec.template.metadata = Some(template_meta);

    let mut pod_spec = PodSpec::default();

    pod_spec.host_pid = Some(true);
    pod_spec.host_ipc = Some(true);
    pod_spec.host_network = Some(true);

    let mut volume = Volume::default();

    volume.name = String::from("host-root");

    let mut host_path = HostPathVolumeSource::default();

    host_path.path = String::from("/");
    volume.host_path = Some(host_path);
    pod_spec.volumes = Some(vec![volume]);

    let mut oneagent_container = Container::default();

    oneagent_container.name = String::from("dynatrace-oneagent");
    oneagent_container.image = Some(String::from("dynatrace/oneagent"));

    let mut installer_env = EnvVar::default();
    let mut cert_check_env = EnvVar::default();
    let mut token_env = EnvVar::default();

    installer_env.name = String::from("ONEAGENT_INSTALLER_SCRIPT_URL");
    installer_env.value = Some(format!(
        "https://{}/api/v1/deployment/installer/agent/unic/default/latest?arch=<arch>",
        api_url
    ));
    cert_check_env.name = String::from("ONEAGENT_INSTALLER_SKIP_CERT_CHECK");
    cert_check_env.value = Some(String::from("false"));
    token_env.name = String::from("ONEAGENT_INSTALLER_DOWNLOAD_TOKEN");
    token_env.value = Some(token.to_string());

    oneagent_container.env = Some(vec![installer_env, cert_check_env, token_env]);
    oneagent_container.args = Some(vec![String::from("--set-network-zone=default")]);

    let mut volume_mount = VolumeMount::default();

    volume_mount.name = String::from("host-root");
    volume_mount.mount_path = String::from("/mnt/root");

    oneagent_container.volume_mounts = Some(vec![volume_mount]);

    let mut security_context = SecurityContext::default();

    security_context.privileged = Some(true);
    oneagent_container.security_context = Some(security_context);

    pod_spec.containers = vec![oneagent_container];
    spec.template.spec = Some(pod_spec);

    return DaemonSet {
        metadata: meta,
        spec: Some(spec),
        status: None,
    };
}
