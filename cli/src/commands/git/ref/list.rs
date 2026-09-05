// Copyright 2025 The Jujutsu Authors
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

use clap_complete::ArgValueCandidates;
use itertools::Itertools as _;
use jj_lib::object_id::ObjectId as _;
use jj_lib::ref_name::RemoteNameBuf;

use super::canonicalize_git_ref_name;
use crate::cli_util::CommandHelper;
use crate::command_error::CommandError;
use crate::complete;
use crate::ui::Ui;

/// List fetched Git refs
#[derive(clap::Args, Clone, Debug)]
pub struct GitRefListArgs {
    /// Show refs fetched from this remote
    #[arg(long, value_name = "REMOTE")]
    #[arg(add = ArgValueCandidates::new(complete::git_remotes))]
    remote: Option<RemoteNameBuf>,

    /// Show this fetched ref
    #[arg(value_name = "REF")]
    ref_name: Option<String>,
}

pub async fn cmd_git_ref_list(
    ui: &mut Ui,
    command: &CommandHelper,
    args: &GitRefListArgs,
) -> Result<(), CommandError> {
    let workspace_command = command.workspace_helper(ui).await?;
    let view = workspace_command.repo().view();
    let remote_filter = args.remote.as_deref();
    let ref_filter = args
        .ref_name
        .as_deref()
        .map(canonicalize_git_ref_name)
        .transpose()?;
    let mut matched = 0;
    let mut has_conflict = false;
    for ((remote_name, ref_name), target) in view.all_fetched_git_refs() {
        if remote_filter.is_some_and(|remote| remote != remote_name) {
            continue;
        }
        if ref_filter.as_ref().is_some_and(|name| name != ref_name) {
            continue;
        }
        matched += 1;
        if target.has_conflict() {
            has_conflict = true;
            writeln!(
                ui.stdout(),
                "{} {} (conflicted):",
                remote_name.as_symbol(),
                ref_name.as_symbol()
            )?;
            for id in target.added_ids() {
                writeln!(ui.stdout(), "  + {}", id.hex())?;
            }
            for id in target.removed_ids() {
                writeln!(ui.stdout(), "  - {}", id.hex())?;
            }
        } else {
            let target_ids = target.added_ids().map(|id| id.hex()).join(", ");
            writeln!(
                ui.stdout(),
                "{} {} {}",
                remote_name.as_symbol(),
                ref_name.as_symbol(),
                target_ids
            )?;
        }
    }
    if matched == 0 {
        writeln!(ui.status(), "No fetched Git refs.")?;
    } else if has_conflict {
        writeln!(
            ui.hint_default(),
            "Some fetched Git refs have conflicts. Fetch them again or forget them to resolve."
        )?;
    }
    Ok(())
}
