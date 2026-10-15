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

mod fetch;
mod forget;
mod list;
mod push;

use clap::Subcommand;

use self::fetch::GitRefFetchArgs;
use self::fetch::cmd_git_ref_fetch;
use self::forget::GitRefForgetArgs;
use self::forget::cmd_git_ref_forget;
use self::list::GitRefListArgs;
use self::list::cmd_git_ref_list;
use self::push::GitRefPushArgs;
use self::push::cmd_git_ref_push;
use crate::cli_util::CommandHelper;
use crate::command_error::CommandError;
use crate::ui::Ui;

/// Manage raw Git refs
#[derive(Subcommand, Clone, Debug)]
pub enum RefCommand {
    Fetch(GitRefFetchArgs),
    Forget(GitRefForgetArgs),
    List(GitRefListArgs),
    Push(GitRefPushArgs),
}

pub async fn cmd_git_ref(
    ui: &mut Ui,
    command: &CommandHelper,
    subcommand: &RefCommand,
) -> Result<(), CommandError> {
    match subcommand {
        RefCommand::Fetch(args) => cmd_git_ref_fetch(ui, command, args).await,
        RefCommand::Forget(args) => cmd_git_ref_forget(ui, command, args).await,
        RefCommand::List(args) => cmd_git_ref_list(ui, command, args).await,
        RefCommand::Push(args) => cmd_git_ref_push(ui, command, args).await,
    }
}
