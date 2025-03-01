//!
//! methods to directly interact with the bdev layer

use crate::{
    context::{Context, OutputFormat},
    ClientError,
    GrpcStatus,
};
use clap::{App, AppSettings, Arg, ArgMatches, SubCommand};
use colored_json::prelude::*;
use mayastor_api::v1 as v1rpc;
use snafu::ResultExt;
use tonic::Status;

pub async fn handler(
    ctx: Context,
    matches: &ArgMatches<'_>,
) -> crate::Result<()> {
    match matches.subcommand() {
        ("list", Some(args)) => list(ctx, args).await,
        ("create", Some(args)) => create(ctx, args).await,
        ("share", Some(args)) => share(ctx, args).await,
        ("destroy", Some(args)) => destroy(ctx, args).await,
        ("unshare", Some(args)) => unshare(ctx, args).await,
        (cmd, _) => {
            Err(Status::not_found(format!("command {cmd} does not exist")))
                .context(GrpcStatus)
        }
    }
}

pub fn subcommands<'a, 'b>() -> App<'a, 'b> {
    let list = SubCommand::with_name("list").about("List all bdevs");
    let create = SubCommand::with_name("create")
        .about("Create a new bdev by specifying a URI")
        .arg(Arg::with_name("uri").required(true).index(1));

    let destroy = SubCommand::with_name("destroy")
        .about("destroy the given bdev")
        .arg(Arg::with_name("name").required(true).index(1));

    let share = SubCommand::with_name("share")
        .about("share the given bdev")
        .arg(Arg::with_name("name").required(true).index(1))
        .arg(
            Arg::with_name("protocol")
                .long("protocol")
                .short("p")
                .help("the protocol to used to share the given bdev.")
                .required(false)
                .possible_values(&["Nvmf"])
                .takes_value(true)
                .default_value("Nvmf"),
        )
        .arg(
            Arg::with_name("allowed-host")
                .long("allowed-host")
                .takes_value(true)
                .multiple(true)
                .required(false)
                .help(
                    "NQN of hosts which are allowed to connect to the target",
                ),
        );

    let unshare = SubCommand::with_name("unshare")
        .about("unshare the given bdev")
        .arg(Arg::with_name("name").required(true).index(1));

    SubCommand::with_name("bdev")
        .settings(&[
            AppSettings::SubcommandRequiredElseHelp,
            AppSettings::ColoredHelp,
            AppSettings::ColorAlways,
        ])
        .about("Block device management")
        .subcommand(list)
        .subcommand(share)
        .subcommand(unshare)
        .subcommand(create)
        .subcommand(destroy)
}

async fn list(mut ctx: Context, _args: &ArgMatches<'_>) -> crate::Result<()> {
    let response = ctx
        .v1
        .bdev
        .list(v1rpc::bdev::ListBdevOptions {
            name: None,
        })
        .await
        .context(GrpcStatus)?;

    match ctx.output {
        OutputFormat::Json => {
            println!(
                "{}",
                serde_json::to_string_pretty(response.get_ref())
                    .unwrap()
                    .to_colored_json_auto()
                    .unwrap()
            );
        }
        OutputFormat::Default => {
            let bdevs = &response.get_ref().bdevs;
            if bdevs.is_empty() {
                ctx.v1("No bdevs found");
                return Ok(());
            }
            let header = vec![
                "UUID",
                "NUM_BLOCKS",
                "BLK_SIZE",
                "CLAIMED_BY",
                "NAME",
                "SHARE_URI",
            ];
            let table = bdevs
                .iter()
                .map(|bdev| {
                    vec![
                        bdev.uuid.to_string(),
                        bdev.num_blocks.to_string(),
                        bdev.blk_size.to_string(),
                        bdev.claimed_by.to_string(),
                        bdev.name.to_string(),
                        bdev.share_uri.to_string(),
                    ]
                })
                .collect();
            ctx.print_list(header, table);
        }
    };

    Ok(())
}

async fn create(mut ctx: Context, args: &ArgMatches<'_>) -> crate::Result<()> {
    let uri = args
        .value_of("uri")
        .ok_or_else(|| ClientError::MissingValue {
            field: "uri".to_string(),
        })?
        .to_owned();

    let response = ctx
        .v1
        .bdev
        .create(v1rpc::bdev::CreateBdevRequest {
            uri,
        })
        .await
        .context(GrpcStatus)?;

    match ctx.output {
        OutputFormat::Json => {
            println!(
                "{}",
                serde_json::to_string_pretty(&response.get_ref())
                    .unwrap()
                    .to_colored_json_auto()
                    .unwrap()
            );
        }
        OutputFormat::Default => {
            println!("{}", &response.get_ref().bdev.as_ref().unwrap().name);
        }
    };

    Ok(())
}

async fn destroy(mut ctx: Context, args: &ArgMatches<'_>) -> crate::Result<()> {
    let name = args
        .value_of("name")
        .ok_or_else(|| ClientError::MissingValue {
            field: "name".to_string(),
        })?
        .to_owned();

    let bdevs = ctx
        .v1
        .bdev
        .list(v1rpc::bdev::ListBdevOptions {
            name: None,
        })
        .await
        .context(GrpcStatus)?
        .into_inner();

    let found = bdevs
        .bdevs
        .iter()
        .find(|b| b.name == name)
        .ok_or_else(|| Status::not_found(name.clone()))
        .context(GrpcStatus)?;

    // un share the bdev
    let _ = ctx
        .v1
        .bdev
        .unshare(v1rpc::bdev::BdevUnshareRequest {
            name,
        })
        .await
        .context(GrpcStatus)?;

    let response = ctx
        .v1
        .bdev
        .destroy(v1rpc::bdev::DestroyBdevRequest {
            uri: found.uri.clone(),
        })
        .await
        .context(GrpcStatus)?;

    match ctx.output {
        OutputFormat::Json => {
            println!(
                "{}",
                serde_json::to_string_pretty(&response.get_ref())
                    .unwrap()
                    .to_colored_json_auto()
                    .unwrap()
            );
        }
        OutputFormat::Default => {
            println!("{}", found.name,);
        }
    };

    Ok(())
}

async fn share(mut ctx: Context, args: &ArgMatches<'_>) -> crate::Result<()> {
    let name = args
        .value_of("name")
        .ok_or_else(|| ClientError::MissingValue {
            field: "name".to_string(),
        })?
        .to_owned();
    let protocol = args
        .value_of("protocol")
        .ok_or_else(|| ClientError::MissingValue {
            field: "protocol".to_string(),
        })?
        .to_owned();
    let allowed_hosts =
        args.values_of_lossy("allowed-host").unwrap_or_default();

    let val = if protocol == "Nvmf" {
        v1rpc::common::ShareProtocol::Nvmf as i32
    } else {
        v1rpc::common::ShareProtocol::None as i32
    };

    let response = ctx
        .v1
        .bdev
        .share(v1rpc::bdev::BdevShareRequest {
            name,
            protocol: val,
            allowed_hosts,
        })
        .await
        .context(GrpcStatus)?;

    match ctx.output {
        OutputFormat::Json => {
            println!(
                "{}",
                serde_json::to_string_pretty(&response.get_ref())
                    .unwrap()
                    .to_colored_json_auto()
                    .unwrap()
            );
        }
        OutputFormat::Default => {
            println!("{}", &response.get_ref().bdev.as_ref().unwrap().uri);
        }
    }
    Ok(())
}

async fn unshare(mut ctx: Context, args: &ArgMatches<'_>) -> crate::Result<()> {
    let name = args
        .value_of("name")
        .ok_or_else(|| ClientError::MissingValue {
            field: "name".to_string(),
        })?
        .to_owned();

    let response = ctx
        .v1
        .bdev
        .unshare(v1rpc::bdev::BdevUnshareRequest {
            name: name.clone(),
        })
        .await
        .context(GrpcStatus)?;

    match ctx.output {
        OutputFormat::Json => {
            println!(
                "{}",
                serde_json::to_string_pretty(&response.get_ref())
                    .unwrap()
                    .to_colored_json_auto()
                    .unwrap()
            );
        }
        OutputFormat::Default => {
            println!("{name}",);
        }
    }
    Ok(())
}
