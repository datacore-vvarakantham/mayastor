use crate::{
    context::{Context, OutputFormat},
    parse_size,
    ClientError,
    GrpcStatus,
};
use byte_unit::Byte;
use clap::{App, AppSettings, Arg, ArgMatches, SubCommand};
use colored_json::ToColoredJson;
use futures::StreamExt;
use mayastor_api::v1 as v1_rpc;
use snafu::ResultExt;
use std::{convert::TryInto, str::FromStr};
use strum::VariantNames;
use strum_macros::{AsRefStr, EnumString, EnumVariantNames};
use tonic::Status;

pub fn subcommands<'a, 'b>() -> App<'a, 'b> {
    let inject = SubCommand::with_name("inject")
        .about("manage fault injections")
        .arg(
            Arg::with_name("add")
                .short("a")
                .long("add")
                .required(false)
                .takes_value(true)
                .multiple(true)
                .number_of_values(1)
                .help("new injection uri"),
        )
        .arg(
            Arg::with_name("remove")
                .short("r")
                .long("remove")
                .required(false)
                .takes_value(true)
                .multiple(true)
                .number_of_values(1)
                .help("injection uri"),
        );

    let wipe = SubCommand::with_name("wipe")
        .about("Wipe Resource")
        .arg(
            Arg::with_name("resource")
                .required(true)
                .index(1)
                .possible_values(Resource::resources())
                .help("Resource to Wipe"),
        )
        .arg(
            Arg::with_name("uuid")
                .required(true)
                .index(2)
                .help("Resource uuid"),
        )
        .arg(
            Arg::with_name("pool-uuid")
                .long("pool-uuid")
                .required(false)
                .takes_value(true)
                .requires_if("resource", Resource::Replica.as_ref())
                .conflicts_with("pool-name")
                .help("Uuid of the pool where the replica resides"),
        )
        .arg(
            Arg::with_name("pool-name")
                .long("pool-name")
                .required(false)
                .takes_value(true)
                .requires_if("resource", Resource::Replica.as_ref())
                .conflicts_with("pool-uuid")
                .help("Name of the pool where the replica resides"),
        )
        .arg(
            Arg::with_name("method")
                .short("m")
                .long("method")
                .takes_value(true)
                .value_name("METHOD")
                .default_value("WriteZeroes")
                .possible_values(WipeMethod::methods())
                .help("Method used to wipe the replica"),
        )
        .arg(
            Arg::with_name("chunk-size")
                .short("c")
                .long("chunk-size")
                .takes_value(true)
                .value_name("CHUNK-SIZE")
                .help("Reporting back stats after each chunk is wiped"),
        );

    SubCommand::with_name("test")
        .settings(&[
            AppSettings::SubcommandRequiredElseHelp,
            AppSettings::ColoredHelp,
            AppSettings::ColorAlways,
        ])
        .about("Test management")
        .subcommand(inject)
        .subcommand(wipe)
}

#[derive(EnumString, EnumVariantNames, AsRefStr)]
#[strum(serialize_all = "camelCase")]
enum Resource {
    Replica,
}
impl Resource {
    fn resources() -> &'static [&'static str] {
        Self::VARIANTS
    }
}

#[derive(EnumString, EnumVariantNames)]
#[strum(serialize_all = "PascalCase")]
enum WipeMethod {
    None,
    WriteZeroes,
    Unmap,
    WritePattern,
}
impl WipeMethod {
    fn methods() -> &'static [&'static str] {
        Self::VARIANTS
    }
}
impl From<WipeMethod> for v1_rpc::test::wipe_options::WipeMethod {
    fn from(value: WipeMethod) -> Self {
        match value {
            WipeMethod::None => Self::None,
            WipeMethod::WriteZeroes => Self::WriteZeroes,
            WipeMethod::Unmap => Self::Unmap,
            WipeMethod::WritePattern => Self::WritePattern,
        }
    }
}

pub async fn handler(
    ctx: Context,
    matches: &ArgMatches<'_>,
) -> crate::Result<()> {
    match matches.subcommand() {
        ("inject", Some(args)) => injections(ctx, args).await,
        ("wipe", Some(args)) => wipe(ctx, args).await,
        (cmd, _) => {
            Err(Status::not_found(format!("command {cmd} does not exist")))
                .context(GrpcStatus)
        }
    }
}

async fn wipe(ctx: Context, matches: &ArgMatches<'_>) -> crate::Result<()> {
    let resource = matches
        .value_of("resource")
        .map(Resource::from_str)
        .ok_or_else(|| ClientError::MissingValue {
            field: "resource".to_string(),
        })?
        .map_err(|e| Status::invalid_argument(e.to_string()))
        .context(GrpcStatus)?;

    match resource {
        Resource::Replica => replica_wipe(ctx, matches).await,
    }
}

async fn replica_wipe(
    mut ctx: Context,
    matches: &ArgMatches<'_>,
) -> crate::Result<()> {
    let uuid = matches
        .value_of("uuid")
        .ok_or_else(|| ClientError::MissingValue {
            field: "uuid".to_string(),
        })?
        .to_owned();

    let pool = match matches.value_of("pool-uuid") {
        Some(uuid) => Some(v1_rpc::test::wipe_replica_request::Pool::PoolUuid(
            uuid.to_string(),
        )),
        None => matches.value_of("pool-name").map(|name| {
            v1_rpc::test::wipe_replica_request::Pool::PoolName(name.to_string())
        }),
    };

    let method_str = matches.value_of("method").ok_or_else(|| {
        ClientError::MissingValue {
            field: "method".to_string(),
        }
    })?;
    let method = WipeMethod::from_str(method_str)
        .map_err(|e| Status::invalid_argument(e.to_string()))
        .context(GrpcStatus)?;

    let chunk_size = parse_size(matches.value_of("chunk-size").unwrap_or("0"))
        .map_err(|s| Status::invalid_argument(format!("Bad size '{s}'")))
        .context(GrpcStatus)?;
    let response = ctx
        .v1
        .test
        .wipe_replica(v1_rpc::test::WipeReplicaRequest {
            uuid,
            pool,
            wipe_options: Some(v1_rpc::test::StreamWipeOptions {
                options: Some(v1_rpc::test::WipeOptions {
                    wipe_method: v1_rpc::test::wipe_options::WipeMethod::from(
                        method,
                    ) as i32,
                    write_pattern: None,
                }),
                chunk_size: chunk_size.get_bytes() as u64,
            }),
        })
        .await
        .context(GrpcStatus)?;

    let mut resp = response.into_inner();

    fn bandwidth(response: &v1_rpc::test::WipeReplicaResponse) -> String {
        let unknown = "??".to_string();
        let Some(Ok(elapsed)) = response
            .since
            .clone()
            .map(TryInto::<std::time::Duration>::try_into)
        else {
            return unknown;
        };
        let elapsed_f = elapsed.as_secs_f64();
        if !elapsed_f.is_normal() {
            return unknown;
        }

        let bandwidth = (response.wiped_bytes as f64 / elapsed_f) as u128;
        format!(
            "{}/s",
            byte_unit::Byte::from_bytes(bandwidth).get_appropriate_unit(true)
        )
    }

    match ctx.output {
        OutputFormat::Json => {
            while let Some(response) = resp.next().await {
                let response = response.context(GrpcStatus)?;
                println!(
                    "{}",
                    serde_json::to_string_pretty(&response)
                        .unwrap()
                        .to_colored_json_auto()
                        .unwrap()
                );
            }
        }
        OutputFormat::Default => {
            let header = vec![
                "UUID",
                "TOTAL_BYTES",
                "CHUNK_SIZE",
                "LAST_CHUNK_SIZE",
                "TOTAL_CHUNKS",
                "WIPED_BYTES",
                "WIPED_CHUNKS",
                "REMAINING_BYTES",
                "BANDWIDTH",
            ];

            let (s, r) = tokio::sync::mpsc::channel(10);
            tokio::spawn(async move {
                while let Some(response) = resp.next().await {
                    let response = response.map(|response| {
                        let bandwidth = bandwidth(&response);
                        vec![
                            response.uuid,
                            adjust_bytes(response.total_bytes),
                            adjust_bytes(response.chunk_size),
                            adjust_bytes(response.last_chunk_size),
                            response.total_chunks.to_string(),
                            adjust_bytes(response.wiped_bytes),
                            response.wiped_chunks.to_string(),
                            adjust_bytes(response.remaining_bytes),
                            bandwidth,
                        ]
                    });
                    s.send(response).await.unwrap();
                }
            });
            ctx.print_streamed_list(header, r)
                .await
                .context(GrpcStatus)?;
        }
    }

    Ok(())
}

fn adjust_bytes(bytes: u64) -> String {
    let byte = Byte::from_bytes(bytes as u128);
    let adjusted_byte = byte.get_appropriate_unit(true);
    adjusted_byte.to_string()
}

async fn injections(
    mut ctx: Context,
    matches: &ArgMatches<'_>,
) -> crate::Result<()> {
    let inj_add = matches.values_of("add");
    let inj_remove = matches.values_of("remove");
    if inj_add.is_none() && inj_remove.is_none() {
        return list_injections(ctx).await;
    }

    if let Some(uris) = inj_add {
        for uri in uris {
            println!("Injection: '{uri}'");
            ctx.v1
                .test
                .add_fault_injection(v1_rpc::test::AddFaultInjectionRequest {
                    uri: uri.to_owned(),
                })
                .await
                .context(GrpcStatus)?;
        }
    }

    if let Some(uris) = inj_remove {
        for uri in uris {
            println!("Removing injected fault: {uri}");
            ctx.v1
                .test
                .remove_fault_injection(
                    v1_rpc::test::RemoveFaultInjectionRequest {
                        uri: uri.to_owned(),
                    },
                )
                .await
                .context(GrpcStatus)?;
        }
    }

    Ok(())
}

async fn list_injections(mut ctx: Context) -> crate::Result<()> {
    let response = ctx
        .v1
        .test
        .list_fault_injections(v1_rpc::test::ListFaultInjectionsRequest {})
        .await
        .context(GrpcStatus)?;

    println!(
        "{}",
        serde_json::to_string_pretty(response.get_ref())
            .unwrap()
            .to_colored_json_auto()
            .unwrap()
    );

    Ok(())
}
