// Copyright 2026 The Jujutsu Authors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// https://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::io::Write as _;

use clap::ArgGroup;
use clap_complete::ArgValueCandidates;
use jj_lib::git;
use jj_lib::git::GitPushOptions;
use jj_lib::git::GitRefUpdate;
use jj_lib::git::GitSettings;
use jj_lib::merge::Diff;
use jj_lib::object_id::ObjectId as _;
use jj_lib::ref_name::GitRefNameBuf;
use jj_lib::ref_name::RemoteNameBuf;
use jj_lib::repo::Repo as _;

use crate::cli_util::CommandHelper;
use crate::cli_util::RevisionArg;
use crate::command_error::CommandError;
use crate::command_error::user_error;
use crate::commands::git::push::get_default_push_remote;
use crate::complete;
use crate::git_util::GitSubprocessUi;
use crate::git_util::print_git_ref_push_stats;
use crate::ui::Ui;

/// Push one revision to a raw Git ref on a remote
///
/// This low-level command is intended for application-specific refs such as
/// Gerrit's `refs/for/main`. It does not create or track a bookmark.
///
/// Each invocation must choose whether to push unconditionally, require the ref
/// to be absent, or require it to point to an expected object ID.
///
/// Unlike `jj git push`, this command does not check for private commits,
/// missing descriptions or identities, or conflicts. It also does not use
/// `git.sign-on-push`; it pushes the selected Git commit as stored.
#[derive(clap::Args, Clone, Debug)]
#[command(group(
    ArgGroup::new("safety")
        .required(true)
        .multiple(false)
        .args(["force", "expected_at", "expected_absent"])
))]
pub struct GitRefPushArgs {
    /// The remote to push to (only named remotes are supported)
    ///
    /// This defaults to the `git.push` setting. If that is not configured, the
    /// only remote or the remote named "origin" is used.
    #[arg(
        long,
        value_name = "REMOTE",
        add = ArgValueCandidates::new(complete::git_remotes),
    )]
    remote: Option<RemoteNameBuf>,

    /// Push without checking the current position of the remote ref
    #[arg(long)]
    force: bool,

    /// Push only if the remote ref is currently at this full Git object ID
    #[arg(long, value_name = "OBJECT_ID")]
    expected_at: Option<gix::ObjectId>,

    /// Push only if the remote ref does not exist
    #[arg(long)]
    expected_absent: bool,

    /// Git push options (can be repeated)
    #[arg(long, short)]
    option: Vec<String>,

    /// The revision to push
    #[arg(value_name = "REVISION")]
    revision: RevisionArg,

    /// The fully qualified destination ref, such as `refs/for/main`
    #[arg(value_name = "REF")]
    ref_name: String,
}

pub async fn cmd_git_ref_push(
    ui: &mut Ui,
    command: &CommandHelper,
    args: &GitRefPushArgs,
) -> Result<(), CommandError> {
    let workspace_command = command.workspace_helper(ui).await?;
    git::get_git_backend(workspace_command.repo().store())?;
    let remote = match &args.remote {
        Some(remote) => remote.clone(),
        None => get_default_push_remote(ui, &workspace_command)?,
    };

    if !args.ref_name.starts_with("refs/")
        || <&gix::refs::FullNameRef>::try_from(args.ref_name.as_str()).is_err()
    {
        return Err(user_error(format!(
            "Invalid fully qualified Git ref name: {}",
            args.ref_name
        )));
    }
    let full_name = GitRefNameBuf::from(args.ref_name.as_str());

    let commit = workspace_command
        .resolve_single_rev(ui, &args.revision)
        .await?;
    if commit.id() == workspace_command.repo().store().root_commit_id() {
        return Err(user_error("Cannot push the root commit to Git"));
    }
    let new_target = gix::ObjectId::from_bytes_or_panic(commit.id().as_bytes());
    let expected_target = args.expected_at;
    if let Some(expected_target) = expected_target
        && expected_target.kind() != new_target.kind()
    {
        return Err(user_error(format!(
            "Git object ID for --expected-at uses {}, but this repository uses {}",
            expected_target.kind(),
            new_target.kind()
        )));
    }

    let expected_target_matches_new_target = expected_target == Some(new_target);
    let update = if args.force {
        GitRefUpdate::forced(full_name.clone(), Some(new_target))
    } else {
        GitRefUpdate::with_lease(
            full_name.clone(),
            Diff::new(expected_target, Some(new_target)),
        )
    };
    let git_settings = GitSettings::from_settings(workspace_command.settings())?;
    let options = GitPushOptions {
        remote_push_options: args.option.clone(),
    };
    let stats = git::push_updates(
        workspace_command.repo().as_ref(),
        git_settings.to_subprocess_options(),
        remote.as_ref(),
        &[update],
        &mut GitSubprocessUi::new(ui),
        &options,
    )?;
    print_git_ref_push_stats(ui, &stats)?;
    if !stats.all_ok() {
        return Err(user_error(format!(
            "Failed to push Git ref {}",
            args.ref_name
        )));
    }

    if stats.up_to_date.contains(&full_name) {
        if !args.force && !expected_target_matches_new_target {
            let expectation = if args.expected_absent {
                "not exist".to_owned()
            } else {
                format!(
                    "point to {}",
                    args.expected_at.expect("safety argument is required")
                )
            };
            return Err(user_error(format!(
                "Git ref {}@{} already points to {}, but it was expected to {expectation}",
                args.ref_name,
                remote.as_symbol(),
                new_target
            )));
        }
        if let Some(mut formatter) = ui.status_formatter() {
            write!(
                formatter,
                "Git ref {ref_name}@{remote} already points to ",
                ref_name = args.ref_name,
                remote = remote.as_symbol()
            )?;
            workspace_command.write_commit_summary(formatter.as_mut(), &commit)?;
            writeln!(formatter)?;
        }
        return Ok(());
    }

    if let Some(mut formatter) = ui.status_formatter() {
        write!(formatter, "Pushed ")?;
        workspace_command.write_commit_summary(formatter.as_mut(), &commit)?;
        writeln!(
            formatter,
            " to {ref_name}@{remote}",
            ref_name = args.ref_name,
            remote = remote.as_symbol()
        )?;
    }
    Ok(())
}
