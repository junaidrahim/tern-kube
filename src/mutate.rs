//! Changes to the cluster: scale, rollout restart, delete, CronJob trigger and
//! suspend, node cordon. Each is one API call (two for a trigger), made the
//! way kubectl makes it.

use kube::{
	Client,
	api::{Api, ApiResource, DeleteParams, DynamicObject, Patch, PatchParams, PostParams},
};
use serde_json::{Value, json};

/// A change.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Mutation {
	/// Set `spec.replicas` through the scale subresource.
	Scale(i64),
	/// `kubectl rollout restart`: stamp the pod template.
	Restart,
	/// Delete.
	Delete,
	/// Create a Job from a CronJob now (`kubectl create job --from`).
	Trigger,
	/// Suspend (`true`) or resume a CronJob.
	Suspend(bool),
	/// Cordon (`true`) or uncordon a node.
	Cordon(bool),
}

impl Mutation {
	/// The verb, for confirmations and results.
	pub fn verb(&self) -> String {
		match self {
			Self::Scale(n) => format!("Scale to {n}"),
			Self::Restart => "Restart".into(),
			Self::Delete => "Delete".into(),
			Self::Trigger => "Trigger".into(),
			Self::Suspend(true) => "Suspend".into(),
			Self::Suspend(false) => "Resume".into(),
			Self::Cordon(true) => "Cordon".into(),
			Self::Cordon(false) => "Uncordon".into(),
		}
	}

	/// Whether it destroys something.
	pub const fn destructive(&self) -> bool {
		matches!(self, Self::Delete)
	}
}

/// What a mutation acts on.
#[derive(Clone, Debug)]
pub struct Object {
	/// Its resource.
	pub ar:        ApiResource,
	/// Its namespace.
	pub namespace: Option<String>,
	/// Its name.
	pub name:      String,
}

impl Object {
	fn api(&self, client: Client) -> Api<DynamicObject> {
		match &self.namespace {
			Some(ns) => Api::namespaced_with(client, ns, &self.ar),
			None => Api::all_with(client, &self.ar),
		}
	}
}

fn describe(e: kube::Error) -> String {
	match e {
		kube::Error::Api(s) => s.message.clone(),
		other => other.to_string(),
	}
}

/// The merge patch `kubectl rollout restart` sends.
pub fn restart_patch(now: &str) -> Value {
	json!({"spec": {"template": {"metadata": {"annotations": {"kubectl.kubernetes.io/restartedAt": now}}}}})
}

/// The Job `kubectl create job --from=cronjob/<name>` creates.
pub fn job_from_cronjob(cron: &DynamicObject, job_name: &str) -> Value {
	let template = cron.data.pointer("/spec/jobTemplate").cloned().unwrap_or_else(|| json!({}));
	let mut labels = template.pointer("/metadata/labels").cloned().unwrap_or_else(|| json!({}));
	let mut annotations = template.pointer("/metadata/annotations").cloned().unwrap_or_else(|| json!({}));
	if let Some(a) = annotations.as_object_mut() {
		a.insert("cronjob.kubernetes.io/instantiate".into(), json!("manual"));
	}
	if !labels.is_object() {
		labels = json!({});
	}
	json!({
		"apiVersion": "batch/v1",
		"kind": "Job",
		"metadata": {
			"name": job_name,
			"namespace": cron.metadata.namespace,
			"labels": labels,
			"annotations": annotations,
			"ownerReferences": [{
				"apiVersion": "batch/v1",
				"kind": "CronJob",
				"name": cron.metadata.name,
				"uid": cron.metadata.uid,
				"controller": true,
			}],
		},
		"spec": template.get("spec").cloned().unwrap_or_else(|| json!({})),
	})
}

/// A Job name for a manual run: `<cron>-manual-<unix seconds>`, cut to 63.
pub fn manual_job_name(cron: &str, now: i64) -> String {
	let suffix = format!("-manual-{now}");
	let keep = 63usize.saturating_sub(suffix.len()).min(cron.len());
	format!("{}{suffix}", &cron[..cron.floor_char_boundary(keep)])
}

/// Runs `m` on `obj`; `Ok` is a short past-tense summary.
pub async fn run(client: Client, obj: &Object, m: &Mutation) -> Result<String, String> {
	let api = obj.api(client.clone());
	let pp = PatchParams::default();
	let label = format!("{}/{}", obj.ar.kind.to_lowercase(), obj.name);
	match m {
		Mutation::Scale(n) => {
			api.patch_scale(&obj.name, &pp, &Patch::Merge(json!({"spec": {"replicas": n}}))).await.map_err(describe)?;
			Ok(format!("Scaled {label} to {n}"))
		},
		Mutation::Restart => {
			let now = jiff::Timestamp::now().to_string();
			api.patch(&obj.name, &pp, &Patch::Merge(restart_patch(&now))).await.map_err(describe)?;
			Ok(format!("Restarted {label}"))
		},
		Mutation::Delete => {
			api.delete(&obj.name, &DeleteParams::default()).await.map_err(describe)?;
			Ok(format!("Deleted {label}"))
		},
		Mutation::Suspend(on) => {
			api.patch(&obj.name, &pp, &Patch::Merge(json!({"spec": {"suspend": on}}))).await.map_err(describe)?;
			Ok(format!("{} {label}", if *on { "Suspended" } else { "Resumed" }))
		},
		Mutation::Cordon(on) => {
			api.patch(&obj.name, &pp, &Patch::Merge(json!({"spec": {"unschedulable": on}}))).await.map_err(describe)?;
			Ok(format!("{} {label}", if *on { "Cordoned" } else { "Uncordoned" }))
		},
		Mutation::Trigger => {
			let cron = api.get(&obj.name).await.map_err(describe)?;
			let name = manual_job_name(&obj.name, jiff::Timestamp::now().as_second());
			let job: DynamicObject = serde_json::from_value(job_from_cronjob(&cron, &name)).map_err(|e| e.to_string())?;
			let jobs_ar = ApiResource::from_gvk_with_plural(&kube::api::GroupVersionKind::gvk("batch", "v1", "Job"), "jobs");
			let ns = obj.namespace.clone().unwrap_or_default();
			let jobs: Api<DynamicObject> = Api::namespaced_with(client, &ns, &jobs_ar);
			jobs.create(&PostParams::default(), &job).await.map_err(describe)?;
			Ok(format!("Created job/{name}"))
		},
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn manual_runs_copy_the_template_and_point_back_at_the_cronjob() {
		let cron: DynamicObject = serde_json::from_value(json!({
			"apiVersion": "batch/v1", "kind": "CronJob",
			"metadata": {"name": "nightly", "namespace": "ops", "uid": "C1"},
			"spec": {"schedule": "0 0 * * *", "jobTemplate": {
				"metadata": {"labels": {"app": "x"}},
				"spec": {"template": {"spec": {"containers": [{"name": "c", "image": "busybox"}]}}}}}
		}))
		.unwrap();
		let job = job_from_cronjob(&cron, "nightly-manual-1");
		assert_eq!(job["metadata"]["namespace"], "ops");
		assert_eq!(job["metadata"]["labels"]["app"], "x");
		assert_eq!(job["metadata"]["annotations"]["cronjob.kubernetes.io/instantiate"], "manual");
		assert_eq!(job["metadata"]["ownerReferences"][0]["uid"], "C1");
		assert_eq!(job["spec"]["template"]["spec"]["containers"][0]["image"], "busybox");
	}

	#[test]
	fn manual_job_names_fit_a_dns_label() {
		assert_eq!(manual_job_name("nightly", 1_700_000_000), "nightly-manual-1700000000");
		let long = "x".repeat(80);
		assert_eq!(manual_job_name(&long, 1_700_000_000).len(), 63);
	}

	#[test]
	fn restart_stamps_the_pod_template() {
		let p = restart_patch("2026-01-01T00:00:00Z");
		assert_eq!(p["spec"]["template"]["metadata"]["annotations"]["kubectl.kubernetes.io/restartedAt"], "2026-01-01T00:00:00Z");
	}
}
