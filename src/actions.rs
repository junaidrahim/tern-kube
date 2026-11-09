//! Actions that need a real terminal: shell, debug container, `kubectl
//! edit`, `kubectl logs -f`, signing in, and Helm rollback/uninstall. Each
//! opens a Tern split beside this pane with `tern split`.

use std::process::{Command, Stdio};

/// An action.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
	/// `kubectl exec -it … -- sh`.
	Shell,
	/// `kubectl debug -it` with an ephemeral busybox container.
	Debug,
	/// `kubectl edit`.
	Edit,
	/// `kubectl logs -f`.
	FollowLogs,
	/// Run kubectl once in a terminal so the context's credential plugin can
	/// sign in interactively (`tsh`, cloud SSO).
	SignIn,
	/// `helm rollback` to a revision.
	HelmRollback(i64),
	/// `helm uninstall`.
	HelmUninstall,
}

/// What an action works on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Target {
	/// Context.
	pub context:   String,
	/// Resource for kubectl (`pods`, `deployments.apps`).
	pub resource:  String,
	/// Namespace.
	pub namespace: Option<String>,
	/// Name (the release name for Helm).
	pub name:      String,
	/// Container, for pods.
	pub container: Option<String>,
	/// Whether it is a pod.
	pub is_pod:    bool,
}

/// The argv of `action` on `t`.
pub fn argv(action: Action, t: &Target) -> Result<Vec<String>, String> {
	if let Action::HelmRollback(_) | Action::HelmUninstall = action {
		let mut v = vec!["helm".to_owned()];
		match action {
			Action::HelmRollback(rev) => v.extend(["rollback".into(), t.name.clone(), rev.to_string()]),
			_ => v.extend(["uninstall".into(), t.name.clone()]),
		}
		v.extend(["--kube-context".into(), t.context.clone()]);
		if let Some(ns) = &t.namespace {
			v.extend(["--namespace".into(), ns.clone()]);
		}
		return Ok(v);
	}
	let mut argv = vec!["kubectl".to_owned(), "--context".into(), t.context.clone()];
	if action == Action::SignIn {
		// Any request makes the credential plugin run, with a terminal to prompt in.
		argv.extend(["version".into()]);
		return Ok(argv);
	}
	if let Some(ns) = &t.namespace {
		argv.extend(["--namespace".into(), ns.clone()]);
	}
	match action {
		Action::Shell | Action::FollowLogs | Action::Debug if !t.is_pod => return Err("Only pods have a shell and logs".into()),
		Action::Shell => {
			argv.extend(["exec".into(), "-it".into(), t.name.clone()]);
			if let Some(c) = &t.container {
				argv.extend(["-c".into(), c.clone()]);
			}
			// bash when the image has it, else sh.
			argv.extend(["--".into(), "sh".into(), "-c".into(), "command -v bash >/dev/null && exec bash || exec sh".into()]);
		},
		Action::Debug => {
			argv.extend(["debug".into(), "-it".into(), t.name.clone(), "--image=busybox:1.36".into()]);
			if let Some(c) = &t.container {
				// Share the target container's process namespace.
				argv.push(format!("--target={c}"));
			}
		},
		Action::FollowLogs => {
			argv.extend(["logs".into(), "-f".into(), "--tail=200".into(), t.name.clone()]);
			if let Some(c) = &t.container {
				argv.extend(["-c".into(), c.clone()]);
			}
		},
		Action::Edit => argv.extend(["edit".into(), format!("{}/{}", t.resource, t.name)]),
		Action::SignIn | Action::HelmRollback(_) | Action::HelmUninstall => unreachable!("handled above"),
	}
	Ok(argv)
}

/// Opens a split beside this pane running the action.
pub fn launch(action: Action, t: &Target) -> Result<(), String> {
	let pane = std::env::var("TERN_PANE").ok().filter(|p| !p.is_empty()).ok_or("Not running in a Tern pane")?;
	let argv = argv(action, t)?;
	let dir = if action == Action::Edit { "right" } else { "down" };
	let mut cmd = Command::new("tern");
	cmd.args(["split", &pane, dir]);
	if action != Action::Edit {
		// Keep the output readable after the command ends.
		cmd.arg("--keep-open");
	}
	cmd.arg("--").args(&argv).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::piped());
	let child = cmd.spawn().map_err(|e| format!("tern split: {e}"))?;
	// Reap it off the UI thread; report nothing on success.
	std::thread::spawn(move || {
		let _ = child.wait_with_output();
	});
	Ok(())
}

#[cfg(test)]
mod tests {
	use super::*;

	fn pod() -> Target {
		Target {
			context:   "dev".into(),
			resource:  "pods".into(),
			namespace: Some("prod".into()),
			name:      "api-1".into(),
			container: Some("app".into()),
			is_pod:    true,
		}
	}

	#[test]
	fn every_command_names_its_context_and_namespace() {
		let shell = argv(Action::Shell, &pod()).unwrap();
		assert_eq!(&shell[..9], ["kubectl", "--context", "dev", "--namespace", "prod", "exec", "-it", "api-1", "-c"]);
		let edit = argv(Action::Edit, &Target { resource: "deployments.apps".into(), is_pod: false, ..pod() });
		assert_eq!(edit.unwrap()[5..], ["edit".to_owned(), "deployments.apps/api-1".to_owned()]);
		let rb = argv(Action::HelmRollback(3), &Target { name: "web".into(), is_pod: false, ..pod() }).unwrap();
		assert_eq!(rb, ["helm", "rollback", "web", "3", "--kube-context", "dev", "--namespace", "prod"]);
	}

	#[test]
	fn non_pods_have_no_shell_logs_or_debug() {
		let node = Target { namespace: None, is_pod: false, resource: "nodes".into(), ..pod() };
		assert!(argv(Action::Shell, &node).is_err());
		assert!(argv(Action::Debug, &node).is_err());
		assert!(argv(Action::FollowLogs, &node).is_err());
		assert_eq!(argv(Action::Edit, &node).unwrap(), ["kubectl", "--context", "dev", "edit", "nodes/api-1"]);
	}
}
