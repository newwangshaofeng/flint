use crate::AgentLaunchCommand;
use crate::history::{AgentHistoryProvider, HistoricalThread};

pub struct DroidHistoryProvider;

impl AgentHistoryProvider for DroidHistoryProvider {
    fn resume_command(
        &self,
        base: &AgentLaunchCommand,
        thread: &HistoricalThread,
        extra_args: &[String],
    ) -> AgentLaunchCommand {
        let mut args = vec!["--resume".to_string(), thread.session_id.to_string()];
        args.extend(extra_args.iter().cloned());
        AgentLaunchCommand {
            command: base.command.clone(),
            args,
            env: base.env.clone(),
            cwd: Some(thread.project_root.clone()),
            initialization_command: base.initialization_command.clone(),
            hidden: base.hidden,
            default_launch_option: base.default_launch_option.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::time::UNIX_EPOCH;

    use gpui::SharedString;

    use super::*;

    #[test]
    fn resume_uses_resume_flag_and_original_project_root() {
        let base = AgentLaunchCommand {
            command: Some("custom-droid".to_string()),
            args: vec!["--new-only".to_string()],
            env: [(
                "FACTORY_HOME_OVERRIDE".to_string(),
                "/factory-home".to_string(),
            )]
            .into_iter()
            .collect(),
            initialization_command: Some("source ~/.profile".to_string()),
            hidden: true,
            ..Default::default()
        };
        let thread = HistoricalThread {
            session_id: SharedString::from("session-a"),
            title: SharedString::from("Droid session"),
            project_root: PathBuf::from("/root"),
            last_activity_at: UNIX_EPOCH,
        };

        let command = DroidHistoryProvider.resume_command(
            &base,
            &thread,
            &["--auto".to_string(), "high".to_string()],
        );

        assert_eq!(command.command.as_deref(), Some("custom-droid"));
        assert_eq!(command.args, ["--resume", "session-a", "--auto", "high"]);
        assert_eq!(command.cwd.as_deref(), Some(Path::new("/root")));
        assert_eq!(
            command.env.get("FACTORY_HOME_OVERRIDE").map(String::as_str),
            Some("/factory-home")
        );
        assert_eq!(
            command.initialization_command.as_deref(),
            Some("source ~/.profile")
        );
        assert!(command.hidden);
    }

    #[test]
    fn resume_without_extra_args_keeps_the_bare_resume_command() {
        let base = AgentLaunchCommand {
            command: Some("droid".to_string()),
            ..Default::default()
        };
        let thread = HistoricalThread {
            session_id: SharedString::from("session-a"),
            title: SharedString::from("Droid session"),
            project_root: PathBuf::from("/root"),
            last_activity_at: UNIX_EPOCH,
        };

        let command = DroidHistoryProvider.resume_command(&base, &thread, &[]);

        assert_eq!(command.args, ["--resume", "session-a"]);
        assert!(command.env.is_empty());
        assert!(command.initialization_command.is_none());
        assert!(!command.hidden);
    }
}
