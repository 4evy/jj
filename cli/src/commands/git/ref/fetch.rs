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

use clap_complete::ArgValueCandidates;
use itertools::Itertools as _;
use jj_lib::commit::Commit;
use jj_lib::git;
use jj_lib::git::GitFetch;
use jj_lib::git::GitSettings;
use jj_lib::git::import_commit;
use jj_lib::op_store::RefTarget;
use jj_lib::ref_name::GitRefNameBuf;
use jj_lib::ref_name::RemoteName;
use jj_lib::ref_name::RemoteNameBuf;
use jj_lib::repo::Repo as _;
use jj_lib::rewrite::merge_commit_trees;

use crate::cli_util::CommandHelper;
use crate::cli_util::WorkspaceCommandHelper;
use crate::cli_util::WorkspaceCommandTransaction;
use crate::command_error::CommandError;
use crate::command_error::user_error;
use crate::commands::git::fetch::get_default_fetch_remotes;
use crate::complete;
use crate::git_util::GitSubprocessUi;
use crate::git_util::load_git_import_options;
use crate::revset_util::parse_bookmark_name;
use crate::ui::Ui;

/// Fetch one raw ref or full commit ID from a Git remote
///
/// The source can be a fully qualified remote ref such as GitHub's
/// `refs/pull/123/head`, or a full Git commit ID
#[derive(clap::Args, Clone, Debug)]
#[command(group(clap::ArgGroup::new("working_copy").multiple(false)))]
pub struct GitRefFetchArgs {
    /// The remote to fetch from (only named remotes are supported)
    ///
    /// By default, this uses the `git.fetch` setting when it matches exactly
    /// one remote. If that is not configured, the only remote or the remote
    /// named "origin" is used.
    #[arg(
        long = "remote",
        value_name = "REMOTE",
        add = ArgValueCandidates::new(complete::git_remotes),
    )]
    remote: Option<RemoteNameBuf>,

    /// The remote ref or full Git commit ID to fetch
    #[arg(value_name = "REF")]
    source: String,

    /// Create and edit a new empty commit on the fetched ref
    #[arg(long, group = "working_copy")]
    new: bool,

    /// Edit the fetched commit directly
    #[arg(long, group = "working_copy")]
    edit: bool,

    /// Set a local bookmark to the fetched commit
    #[arg(long, value_name = "NAME", value_parser = parse_bookmark_name)]
    bookmark: Option<jj_lib::ref_name::RefNameBuf>,

    /// Allow --bookmark to move an existing local bookmark
    #[arg(long, requires = "bookmark")]
    replace: bool,
}

#[tracing::instrument(skip(ui, command))]
pub async fn cmd_git_ref_fetch(
    ui: &mut Ui,
    command: &CommandHelper,
    args: &GitRefFetchArgs,
) -> Result<(), CommandError> {
    let mut workspace_command = command.workspace_helper(ui).await?;
    let remote_name = if let Some(remote_name) = &args.remote {
        remote_name.clone()
    } else {
        get_default_fetch_remote(ui, &workspace_command)?
    };
    let mut tx = workspace_command.start_transaction();
    let commit = do_fetch(ui, &mut tx, remote_name.as_ref(), &args.source).await?;

    let fetched_ref_name: GitRefNameBuf = args.source.as_str().into();
    tx.repo_mut().set_fetched_git_ref_target(
        remote_name.as_ref(),
        fetched_ref_name.as_ref(),
        RefTarget::normal(commit.id().clone()),
    );

    if let Some(bookmark_name) = &args.bookmark {
        let existing_target = tx.repo().view().get_local_bookmark(bookmark_name);
        if existing_target.is_present()
            && existing_target.as_normal() != Some(commit.id())
            && !args.replace
        {
            return Err(user_error(format!(
                "Bookmark already exists: {name}",
                name = bookmark_name.as_symbol()
            ))
            .hinted("Use --replace to move it to the fetched ref."));
        }
        tx.repo_mut()
            .set_local_bookmark_target(bookmark_name, RefTarget::normal(commit.id().clone()));
    }

    if args.edit {
        tx.base_workspace_helper()
            .check_rewritable([commit.id()])
            .await?;
        tx.edit(&commit)?;
    } else if args.new {
        let merged_tree = merge_commit_trees(tx.repo(), std::slice::from_ref(&commit)).await?;
        let new_commit = tx
            .repo_mut()
            .new_commit(vec![commit.id().clone()], merged_tree)
            .write()
            .await?;
        tx.edit(&new_commit)?;
        if let Some(mut formatter) = ui.status_formatter() {
            write!(formatter, "Created new commit ")?;
            tx.write_commit_summary(formatter.as_mut(), &new_commit)?;
            writeln!(formatter)?;
        }
    }

    tx.finish(
        ui,
        format!(
            "fetch ref or commit ID {} from git remote {}",
            args.source,
            remote_name.as_symbol()
        ),
    )
    .await?;
    Ok(())
}

fn get_default_fetch_remote(
    ui: &Ui,
    workspace_command: &WorkspaceCommandHelper,
) -> Result<RemoteNameBuf, CommandError> {
    let remote_expr = get_default_fetch_remotes(ui, workspace_command)?;
    let remote_matcher = remote_expr.to_matcher();
    let matching_remotes = git::get_all_remote_names(workspace_command.repo().store())?
        .into_iter()
        .filter(|remote| remote_matcher.is_match(remote.as_str()))
        .collect_vec();
    match matching_remotes.as_slice() {
        [] => Err(user_error("No git remotes to fetch from")),
        [remote] => Ok(remote.clone()),
        _ => Err(
            user_error("Default git fetch configuration matches multiple remotes")
                .hinted("Use `--remote` to select one remote for `jj git ref fetch`."),
        ),
    }
}

async fn do_fetch(
    ui: &mut Ui,
    tx: &mut WorkspaceCommandTransaction<'_>,
    remote_name: &RemoteName,
    source: &str,
) -> Result<Commit, CommandError> {
    let remote_settings = tx.settings().remote_settings()?;
    let git_settings = GitSettings::from_settings(tx.settings())?;
    let import_options = load_git_import_options(ui, &git_settings, &remote_settings)?;
    let mut git_fetch = GitFetch::new(
        tx.repo_mut(),
        git_settings.to_subprocess_options(),
        &import_options,
    )?;

    let mut callback = GitSubprocessUi::new(ui);
    let commit_id = git_fetch.fetch_commit(remote_name, source, &mut callback, None)?;
    let commit = import_commit(tx.repo_mut(), commit_id).await?;
    if let Some(mut formatter) = ui.status_formatter() {
        write!(formatter, "Fetched {source} as ")?;
        tx.write_commit_summary(formatter.as_mut(), &commit)?;
        writeln!(formatter)?;
    }

    Ok(commit)
}
