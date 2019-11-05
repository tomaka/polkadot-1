// Copyright 2017 Parity Technologies (UK) Ltd.
// This file is part of Polkadot.

// Polkadot is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Polkadot is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Polkadot.  If not, see <http://www.gnu.org/licenses/>.

//! Polkadot CLI library.

#![warn(missing_docs)]
#![warn(unused_extern_crates)]

mod chain_spec;

use chain_spec::ChainSpec;
use futures::Future;
use tokio::runtime::Runtime;
use std::sync::Arc;
use log::info;
use structopt::StructOpt;

pub use service::{
	AbstractService, CustomConfiguration,
	ProvideRuntimeApi, CoreApi, ParachainHost,
};

pub use cli::{VersionInfo, IntoExit, NoCustom};
pub use cli::{display_role, error};

/// Abstraction over an executor that lets you spawn tasks in the background.
pub type TaskExecutor = Arc<dyn futures::future::Executor<Box<dyn Future<Item = (), Error = ()> + Send>> + Send + Sync>;

fn load_spec(id: &str) -> Result<Option<service::ChainSpec>, String> {
	Ok(match ChainSpec::from(id) {
		Some(spec) => Some(spec.load()?),
		None => None,
	})
}

/// Additional worker making use of the node, to run asynchronously before shutdown.
///
/// This will be invoked with the service and spawn a future that resolves
/// when complete.
pub trait Worker: IntoExit {
	/// A future that resolves when the work is done or the node should exit.
	/// This will be run on a tokio runtime.
	type Work: Future<Item=(),Error=()> + Send + 'static;

	/// Return configuration for the polkadot node.
	// TODO: make this the full configuration, so embedded nodes don't need
	// string CLI args (https://github.com/paritytech/polkadot/issues/111)
	fn configuration(&self) -> service::CustomConfiguration { Default::default() }

	/// Do work and schedule exit.
	fn work<S, SC, B, CE>(self, service: &S, executor: TaskExecutor) -> Self::Work
	where S: AbstractService<Block = service::Block, RuntimeApi = service::RuntimeApi,
		Backend = B, SelectChain = SC,
		NetworkSpecialization = service::PolkadotProtocol, CallExecutor = CE>,
		SC: service::SelectChain<service::Block> + 'static,
		B: service::Backend<service::Block, service::Blake2Hasher> + 'static,
		CE: service::CallExecutor<service::Block, service::Blake2Hasher> + Clone + Send + Sync + 'static;
}

#[derive(Debug, StructOpt, Clone)]
enum PolkadotSubCommands {
	#[structopt(name = "validation-worker", setting = structopt::clap::AppSettings::Hidden)]
	ValidationWorker(ValidationWorkerCommand),
}

impl cli::GetLogFilter for PolkadotSubCommands {
	fn get_log_filter(&self) -> Option<String> { None }
}

#[derive(Debug, StructOpt, Clone)]
struct ValidationWorkerCommand {
	#[structopt()]
	pub mem_id: String,
}

/// Returned by the [`run`] function. Allows performing the actual running.
#[must_use]
pub enum Run<'a> {
	/// The user wants to start the Polkadot node.
	Node(NodeRun<'a>),
	/// The user wants to execute a sub-command (exporting/importing blocks, purging chain, ...).
	Other(OtherRun<'a>),
}

/// Returned by the [`run`] function when the user wants to start the Polkadot node. Allows
/// performing the actual running.
pub struct NodeRun<'a> {
	version: &'a cli::VersionInfo,
	inner: cli::ParseAndPrepareRun<'a, NoCustom>,
}

/// Returned by the [`run`] function when the user wants to execute a sub-command. Allows
/// performing the actual running.
pub struct OtherRun<'a> {
	inner: OtherRunInner<'a>,
}

enum OtherRunInner<'a> {
	BuildSpec(cli::ParseAndPrepareBuildSpec<'a>),
	Export(cli::ParseAndPrepareExport<'a>),
	Import(cli::ParseAndPrepareImport<'a>),
	Purge(cli::ParseAndPreparePurge<'a>),
	Revert(cli::ParseAndPrepareRevert<'a>),
	ValidationWorker(ValidationWorkerCommand),
}

/// Parses polkadot specific CLI arguments and returns a `Run` object corresponding to what the
/// user passed as CLI options.
pub fn run(version: &cli::VersionInfo) -> Run {
	let cmd = cli::parse_and_prepare::<PolkadotSubCommands, NoCustom, _>(
		&version,
		"parity-polkadot",
		std::env::args()
	);

	match cmd {
		cli::ParseAndPrepare::Run(inner) => Run::Node(NodeRun {
			version,
			inner,
		}),
		cli::ParseAndPrepare::BuildSpec(cmd) => Run::Other(OtherRun {
			inner: OtherRunInner::BuildSpec(cmd),
		}),
		cli::ParseAndPrepare::ExportBlocks(cmd) => Run::Other(OtherRun {
			inner: OtherRunInner::Export(cmd),
		}),
		cli::ParseAndPrepare::ImportBlocks(cmd) => Run::Other(OtherRun {
			inner: OtherRunInner::Import(cmd),
		}),
		cli::ParseAndPrepare::PurgeChain(cmd) => Run::Other(OtherRun {
			inner: OtherRunInner::Purge(cmd),
		}),
		cli::ParseAndPrepare::RevertChain(cmd) => Run::Other(OtherRun {
			inner: OtherRunInner::Revert(cmd),
		}),
		cli::ParseAndPrepare::CustomCommand(PolkadotSubCommands::ValidationWorker(cmd)) =>
			Run::Other(OtherRun {
				inner: OtherRunInner::ValidationWorker(cmd),
			}),
	}
}

impl<'a> NodeRun<'a> {
	/// Runs the command. Runs the node until the `until` is triggered.
	pub fn run_until(
		self,
		custom_config: service::CustomConfiguration,
		until: impl cli::IntoExit
	) -> error::Result<()> {
		let version = self.version;
		self.inner.run(load_spec, until, |until, _cli_args, _custom_args, mut config| {
			info!("{}", version.name);
			info!("  version {}", config.full_version());
			info!("  by {}, 2017-2019", version.author);
			info!("Chain specification: {}", config.chain_spec.name());
			info!("Node name: {}", config.name);
			info!("Roles: {}", display_role(&config));
			config.custom = custom_config;
			let runtime = Runtime::new().map_err(|e| format!("{:?}", e))?;
			match config.roles {
				service::Roles::LIGHT =>
					run_until_exit(
						runtime,
						service::new_light(config).map_err(|e| format!("{:?}", e))?,
						until
					),
				_ => run_until_exit(
						runtime,
						service::new_full(config).map_err(|e| format!("{:?}", e))?,
						until
					),
			}.map_err(|e| format!("{:?}", e))
		})
	}
}

impl<'a> OtherRun<'a> {
	/// Runs the other command.
	pub fn run_until(self, until: impl cli::IntoExit) -> error::Result<()> {
		match self.inner {
			OtherRunInner::BuildSpec(cmd) => cmd.run(load_spec),
			OtherRunInner::Export(cmd) => cmd.run_with_builder::<(), _, _, _, _, _, _>(|config|
				Ok(service::new_chain_ops(config)?), load_spec, until),
			OtherRunInner::Import(cmd) => cmd.run_with_builder::<(), _, _, _, _, _, _>(|config|
				Ok(service::new_chain_ops(config)?), load_spec, until),
			OtherRunInner::Purge(cmd) => cmd.run(load_spec),
			OtherRunInner::Revert(cmd) => cmd.run_with_builder::<(), _, _, _, _, _>(|config|
				Ok(service::new_chain_ops(config)?), load_spec),
			OtherRunInner::ValidationWorker(args) => {
				service::run_validation_worker(&args.mem_id)?;
				Ok(())
			}
		}
	}
}

fn run_until_exit<T, SC, B, CE, W>(
	mut runtime: Runtime,
	service: T,
	until: W,
) -> error::Result<()>
	where
		T: AbstractService<Block = service::Block, RuntimeApi = service::RuntimeApi,
			SelectChain = SC, Backend = B, NetworkSpecialization = service::PolkadotProtocol, CallExecutor = CE>,
		SC: service::SelectChain<service::Block> + 'static,
		B: service::Backend<service::Block, service::Blake2Hasher> + 'static,
		CE: service::CallExecutor<service::Block, service::Blake2Hasher> + Clone + Send + Sync + 'static,
		W: IntoExit,
{
	let (exit_send, exit) = exit_future::signal();

	let informant = cli::informant::build(&service);
	runtime.executor().spawn(exit.until(informant).map(|_| ()));

	// we eagerly drop the service so that the internal exit future is fired,
	// but we need to keep holding a reference to the global telemetry guard
	let _telemetry = service.telemetry();

	let exit = until.into_exit().map_err(|_| error::Error::Other("Exit future failed.".into()));
	let service = service.map_err(|err| error::Error::Service(err));
	let select = service.select(exit).map(|_| ()).map_err(|(err, _)| err);
	let _ = runtime.block_on(select);
	exit_send.fire();

	Ok(())
}
