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

use std::num::NonZeroU32;

use clap_complete::ArgValueCandidates;
use itertools::Itertools as _;
use jj_lib::commit::Commit;
use jj_lib::git;
use jj_lib::git::GitFetch;
use jj_lib::git::GitSettings;
use jj_lib::git::get_git_backend;
use jj_lib::git::import_commit;
use jj_lib::op_store::RefTarget;
use jj_lib::ref_name::GitRefNameBuf;
use jj_lib::ref_name::RemoteName;
use jj_lib::ref_name::RemoteNameBuf;
use jj_lib::repo::Repo as _;
use jj_lib::rewrite::merge_commit_trees;

use super::canonicalize_git_ref_name;
use super::qualify_git_ref_name;
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

/// Fetch refs or commits from a Git remote
///
/// A source can be a fully qualified remote ref such as GitHub's
/// `refs/pull/123/head`, a ref pattern containing one `*`, or a full Git commit
/// ID.
#[derive(clap::Args, Clone, Debug)]
#[command(group(clap::ArgGroup::new("working_copy").multiple(false)))]
pub struct GitRefFetchArgs {
    /// The remote to fetch from (only named remotes are supported)
    ///
    /// By default, this uses the `git.fetch` setting when it matches exactly
    /// one remote. If that is not configured, the only remote or the remote
    /// named "origin" is used.
    #[arg(
        long,
        value_name = "REMOTE",
        add = ArgValueCandidates::new(complete::git_remotes),
    )]
    remote: Option<RemoteNameBuf>,

    /// Refs, ref patterns, or full Git commit IDs to fetch
    ///
    /// Ref names may be fully qualified (`refs/pull/123/head`) or omit the
    /// leading `refs/` (`pull/123/head`). A ref pattern may contain one `*`.
    #[arg(value_name = "REF", required = true, num_args = 1..)]
    sources: Vec<String>,

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

    /// Limit fetching to the specified number of commits from each target
    ///
    /// In an existing shallow repository, this defaults to the
    /// `git.fetch-depth` setting when configured.
    #[arg(long, conflicts_with = "shallow_exclude")]
    depth: Option<NonZeroU32>,

    /// Fetch the complete stack after this ref, plus one parent generation
    ///
    /// This is only supported in an existing shallow repository.
    #[arg(long, value_name = "REF", conflicts_with = "depth")]
    shallow_exclude: Option<String>,
}

#[derive(Clone, Debug)]
enum ParsedFetchSource {
    Ref(GitRefNameBuf),
    Pattern(String),
    Object(String),
}

fn parse_fetch_source(
    value: &str,
    object_hash: gix::hash::Kind,
) -> Result<ParsedFetchSource, CommandError> {
    if let Ok(object_id) = gix::ObjectId::from_hex(value.as_bytes()) {
        if object_id.kind() != object_hash {
            return Err(user_error(format!(
                "Git object ID uses {}, but this repository uses {}",
                object_id.kind(),
                object_hash
            )));
        }
        return Ok(ParsedFetchSource::Object(value.to_owned()));
    }

    let canonical_name = qualify_git_ref_name(value);
    if canonical_name.contains('*') {
        if canonical_name.matches('*').count() != 1
            || gix::refs::FullName::try_from(canonical_name.replace('*', "x").as_str()).is_err()
        {
            return Err(user_error(format!("Invalid Git ref pattern: {value}")));
        }
        Ok(ParsedFetchSource::Pattern(canonical_name))
    } else {
        canonicalize_git_ref_name(value).map(ParsedFetchSource::Ref)
    }
}

struct FetchedTarget {
    ref_name: Option<GitRefNameBuf>,
    commit: Commit,
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
    let git_repo = get_git_backend(workspace_command.repo().store())?.git_repo();
    let fetch_sources = args
        .sources
        .iter()
        .map(|value| parse_fetch_source(value, git_repo.object_hash()))
        .collect::<Result<Vec<_>, _>>()?;
    let has_single_target_option = args.new || args.edit || args.bookmark.is_some();
    if has_single_target_option
        && (fetch_sources.len() != 1 || matches!(&fetch_sources[0], ParsedFetchSource::Pattern(_)))
    {
        return Err(user_error(
            "--new, --edit, and --bookmark require exactly one non-pattern fetch target",
        ));
    }
    if args.shallow_exclude.is_some() && !git_repo.is_shallow() {
        return Err(user_error(
            "--shallow-exclude is only supported in an existing shallow repository",
        ));
    }
    let depth = super::super::get_git_fetch_depth(
        workspace_command.settings(),
        workspace_command.repo().store(),
        args.depth,
    )?;

    let mut tx = workspace_command.start_transaction();
    let index_store = tx.repo().base_repo().index_store().clone();
    let (fetch_result, shallow_boundary_changed) = do_fetch(
        ui,
        &mut tx,
        remote_name.as_ref(),
        &fetch_sources,
        depth,
        args.shallow_exclude.as_deref(),
    )
    .await;

    let result: Result<(), CommandError> = async {
        let fetched_targets = fetch_result?;
        for target in &fetched_targets {
            if let Some(ref_name) = &target.ref_name {
                tx.repo_mut().set_fetched_git_ref_target(
                    remote_name.as_ref(),
                    ref_name.as_ref(),
                    RefTarget::normal(target.commit.id().clone()),
                );
            }
        }
        let commit = &fetched_targets[0].commit;

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
            tx.check_rewritable([commit.id()]).await?;
            tx.edit(commit)?;
        } else if args.new {
            let merged_tree = merge_commit_trees(tx.repo(), std::slice::from_ref(commit)).await?;
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
                "fetch refs or commit IDs {} from git remote {}",
                args.sources.iter().join(", "),
                remote_name.as_symbol()
            ),
        )
        .await?;
        Ok(())
    }
    .await;
    let reinit_result = super::super::reinit_index_after_shallow_change(
        ui,
        index_store.as_ref(),
        shallow_boundary_changed,
    );
    result?;
    reinit_result
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
    sources: &[ParsedFetchSource],
    depth: Option<NonZeroU32>,
    shallow_exclude: Option<&str>,
) -> (Result<Vec<FetchedTarget>, CommandError>, bool) {
    let setup = (|| {
        let remote_settings = tx.settings().remote_settings()?;
        let git_settings = GitSettings::from_settings(tx.settings())?;
        let import_options = load_git_import_options(ui, &git_settings, &remote_settings)?;
        Ok::<_, CommandError>((git_settings, import_options))
    })();
    let (git_settings, import_options) = match setup {
        Ok(value) => value,
        Err(err) => return (Err(err), false),
    };
    let mut git_fetch = match GitFetch::new(
        tx.repo_mut(),
        git_settings.to_subprocess_options(),
        &import_options,
    ) {
        Ok(fetcher) => fetcher,
        Err(err) => return (Err(err.into()), false),
    };

    let mut callback = GitSubprocessUi::new(ui);
    let fetch_result: Result<Vec<_>, CommandError> = (|| {
        let mut fetched_ids = Vec::new();
        for source in sources {
            match source {
                ParsedFetchSource::Ref(ref_name) => {
                    let commit_id = git_fetch.fetch_commit(
                        remote_name,
                        ref_name.as_str(),
                        &mut callback,
                        depth,
                        shallow_exclude,
                    )?;
                    fetched_ids.push((
                        Some(ref_name.clone()),
                        ref_name.as_str().to_owned(),
                        commit_id,
                    ));
                }
                ParsedFetchSource::Pattern(pattern) => {
                    let matches = git_fetch.fetch_ref_pattern(
                        remote_name,
                        pattern,
                        &mut callback,
                        depth,
                        shallow_exclude,
                    )?;
                    fetched_ids.extend(matches.into_iter().map(|(ref_name, commit_id)| {
                        (
                            Some(ref_name.clone()),
                            ref_name.as_str().to_owned(),
                            commit_id,
                        )
                    }));
                }
                ParsedFetchSource::Object(object_id) => {
                    let commit_id = git_fetch.fetch_commit(
                        remote_name,
                        object_id,
                        &mut callback,
                        depth,
                        shallow_exclude,
                    )?;
                    fetched_ids.push((None, object_id.clone(), commit_id));
                }
            }
        }
        fetched_ids.sort_by(|left, right| left.1.cmp(&right.1));
        fetched_ids.dedup_by(|left, right| left.1 == right.1);
        Ok(fetched_ids)
    })();
    let shallow_boundary_changed = git_fetch.shallow_boundary_changed();
    drop(git_fetch);
    let result: Result<Vec<FetchedTarget>, CommandError> = async {
        let fetched_ids = fetch_result?;
        let mut targets = Vec::with_capacity(fetched_ids.len());
        for (ref_name, source, commit_id) in fetched_ids {
            let commit = import_commit(tx.repo_mut(), commit_id).await?;
            if let Some(mut formatter) = ui.status_formatter() {
                write!(formatter, "Fetched {source} as ")?;
                tx.write_commit_summary(formatter.as_mut(), &commit)?;
                writeln!(formatter)?;
            }
            targets.push(FetchedTarget { ref_name, commit });
        }
        Ok(targets)
    }
    .await;
    (result, shallow_boundary_changed)
}
