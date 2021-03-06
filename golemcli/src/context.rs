#![allow(dead_code)]

use super::CliArgs;
pub use failure::Fallible;
use futures::prelude::*;
use serde::Serialize;
use std::convert::TryFrom;
use std::path::PathBuf;

pub struct ResponseTable {
    pub columns: Vec<String>,
    pub values: Vec<serde_json::Value>,
}

impl ResponseTable {
    pub fn sort_by(mut self, arg_key: &Option<impl AsRef<str>>) -> Self {
        let key = match arg_key {
            None => return self,
            Some(k) => k.as_ref(),
        };
        let idx =
            match self
                .columns
                .iter()
                .enumerate()
                .find_map(|(idx, v)| if v == key { Some(idx) } else { None })
            {
                None => return self,
                Some(idx) => idx,
            };
        self.values
            .sort_by_key(|v| Some(v.as_array()?.get(idx)?.to_string()));
        self
    }

    pub fn with_summary(self, summary: Vec<serde_json::Value>) -> CommandResponse {
        CommandResponse::Table {
            columns: self.columns,
            values: self.values,
            summary,
        }
    }
}

pub trait FormattedObject {
    fn to_json(&self) -> Fallible<serde_json::Value>;

    fn print(&self) -> Fallible<()>;
}

pub enum CommandResponse {
    NoOutput,
    Object(serde_json::Value),
    Table {
        columns: Vec<String>,
        values: Vec<serde_json::Value>,
        summary: Vec<serde_json::Value>,
    },
    FormattedObject(Box<dyn FormattedObject>),
}

impl CommandResponse {
    pub fn object<T: Serialize>(value: T) -> Fallible<Self> {
        Ok(CommandResponse::Object(serde_json::to_value(value)?))
    }
}

impl From<ResponseTable> for CommandResponse {
    fn from(table: ResponseTable) -> Self {
        CommandResponse::Table {
            columns: table.columns,
            values: table.values,
            summary: Vec::new(),
        }
    }
}

pub struct CliCtx {
    rpc_addr: (String, u16),
    data_dir: PathBuf,
    json_output: bool,
    accept_any_prompt: bool,
    net: Option<Net>,
    interactive: bool,
}

impl TryFrom<&CliArgs> for CliCtx {
    type Error = failure::Error;

    fn try_from(value: &CliArgs) -> Result<Self, Self::Error> {
        let data_dir = value.get_data_dir();
        let rpc_addr = value.get_rcp_address()?;
        let json_output = value.json;
        let net = value.net.clone();
        let accept_any_prompt = value.accept_any_prompt;
        #[cfg(feature = "interactive_cli")]
        let interactive = value.interactive;
        #[cfg(not(feature = "interactive_cli"))]
        let interactive = false;

        Ok(CliCtx {
            rpc_addr,
            data_dir,
            json_output,
            accept_any_prompt,
            net,
            interactive,
        })
    }
}

async fn wait_for_server(
    endpoint: impl actix_wamp::PubSubEndpoint + Clone + 'static,
) -> Result<bool, actix_wamp::Error> {
    use futures::stream::Stream;

    eprintln!("Waiting for server start");
    let subscribe = endpoint.subscribe("golem.rpc_ready");
    futures::pin_mut!(subscribe);
    let _ = subscribe.try_next().await?;
    Ok(true)
}

impl CliCtx {
    pub async fn unlock_app(
        &mut self,
        endpoint: impl actix_wamp::RpcEndpoint + actix_wamp::PubSubEndpoint + Clone + 'static,
    ) -> Fallible<impl actix_wamp::RpcEndpoint + Clone> {
        let is_unlocked = endpoint.as_golem().is_account_unlocked().await?;
        let mut wait_for_start = false;

        if !is_unlocked {
            eprintln!("account locked");
            crate::account::account_unlock(endpoint.clone()).await?;
            wait_for_start = true;
        }

        let are_terms_accepted = endpoint.as_golem_terms().are_terms_accepted().await?;

        if !are_terms_accepted {
            use crate::terms::*;
            use promptly::Promptable;
            eprintln!("Terms is not accepted");

            loop {
                match TermsQuery::prompt("Accept terms ? [(s)how / (a)ccept / (r)eject]") {
                    TermsQuery::Show => {
                        eprintln!("{}", get_terms_text(&endpoint).await?);
                    }
                    TermsQuery::Reject => {
                        return Err(failure::err_msg("terms not accepted"));
                    }
                    TermsQuery::Accept => {
                        break;
                    }
                }
            }
            let enable_monitor = self.prompt_for_acceptance(
                "Enable monitor",
                Some("monitor will be ENABLED"),
                Some("monitor will be DISABLED"),
            );
            let enable_talkback = self.prompt_for_acceptance(
                "Enable talkback",
                Some("talkback will be ENABLED"),
                Some("talkback will be DISABLED"),
            );

            let _ = endpoint
                .as_golem_terms()
                .accept_terms(Some(enable_monitor), Some(enable_talkback))
                .await?;
            wait_for_start = true;
        }

        if wait_for_start {
            let _ = wait_for_server(endpoint.clone()).await?;
        }

        let _ = PROMPT_FLAG.store(self.accept_any_prompt, std::sync::atomic::Ordering::Relaxed);

        Ok(endpoint)
    }

    pub async fn connect_to_app(
        &mut self,
    ) -> Fallible<impl actix_wamp::RpcEndpoint + actix_wamp::PubSubEndpoint + Clone> {
        let (address, port) = &self.rpc_addr;

        let endpoint = golem_rpc_api::connect_to_app(
            &self.data_dir,
            self.net.clone(),
            Some((address.as_str(), *port)),
        )
        .await?;

        Ok(endpoint)
    }

    pub fn message(&mut self, message: &str) {
        eprintln!("{}", message);
    }

    pub fn output(&self, resp: CommandResponse) {
        match resp {
            CommandResponse::NoOutput => {}
            CommandResponse::Table {
                columns,
                values,
                summary,
            } => {
                if self.json_output {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&serde_json::json!({
                            "headers": columns,
                            "values": values
                        }))
                        .unwrap()
                    )
                } else {
                    print_table(columns, values, summary);
                }
            }
            CommandResponse::Object(v) => {
                if self.json_output {
                    println!("{}", serde_json::to_string_pretty(&v).unwrap())
                } else {
                    match v {
                        serde_json::Value::String(s) => {
                            println!("{}", s);
                        }
                        v => println!("{}", serde_yaml::to_string(&v).unwrap()),
                    }
                }
            }
            CommandResponse::FormattedObject(formatted_object) => {
                if self.json_output {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&formatted_object.to_json().unwrap()).unwrap()
                    )
                } else {
                    formatted_object.print().unwrap()
                }
            }
        }
    }

    pub fn prompt_for_acceptance(
        &self,
        msg: &str,
        msg_on_accept: Option<&str>,
        msg_on_reject: Option<&str>,
    ) -> bool {
        if self.accept_any_prompt && !self.interactive {
            return true;
        }
        let enabled = promptly::prompt_default(msg, true);

        if enabled && msg_on_accept.is_some() {
            eprintln!("\t {}", msg_on_accept.unwrap());
        } else if !enabled && msg_on_reject.is_some() {
            eprintln!("\t {}", msg_on_reject.unwrap());
        }
        enabled
    }

    pub fn get_golem_lock_path(&self, is_mainnet: bool) -> PathBuf {
        let dir = match is_mainnet {
            true => "mainnet",
            false => "rinkeby",
        };

        self.data_dir.join(PathBuf::from(dir).join("LOCK"))
    }
}

pub fn create_table<'a>(columns: impl IntoIterator<Item = &'a str>) -> prettytable::Table {
    use prettytable::*;
    let mut table = Table::new();
    //table.set_format(*format::consts::FORMAT_NO_LINESEP_WITH_TITLE);
    table.set_format(*FORMAT_BASIC);

    table.set_titles(Row::new(
        columns
            .into_iter()
            .map(|c| {
                Cell::new(c)
                    //.with_style(Attr::Bold)
                    .with_style(Attr::ForegroundColor(color::GREEN))
            })
            .collect(),
    ));

    table
}

fn print_table(
    columns: Vec<String>,
    values: Vec<serde_json::Value>,
    summary: Vec<serde_json::Value>,
) {
    use prettytable::*;
    let mut table = Table::new();
    //table.set_format(*format::consts::FORMAT_NO_LINESEP_WITH_TITLE);
    table.set_format(*FORMAT_BASIC);

    table.set_titles(Row::new(
        columns
            .iter()
            .map(|c| {
                Cell::new(c)
                    .with_style(Attr::Bold)
                    .with_style(Attr::ForegroundColor(color::GREEN))
            })
            .collect(),
    ));
    if values.is_empty() {
        let _ = table.add_row(columns.iter().map(|_| Cell::new("")).collect());
    }
    for row in values {
        if let Some(row_items) = row.as_array() {
            use serde_json::Value;

            let row_strings = row_items
                .iter()
                .map(|v| match v {
                    Value::String(s) => s.to_string(),
                    Value::Null => "".into(),
                    v => v.to_string(),
                })
                .collect();
            table.add_row(row_strings);
        }
    }
    if !summary.is_empty() {
        table.add_row(Row::empty());
        table.add_empty_row();
        let l = summary.len();
        for (idx, row) in summary.into_iter().enumerate() {
            if let Some(row_items) = row.as_array() {
                use serde_json::Value;

                let row_strings = Row::new(
                    row_items
                        .iter()
                        .map(|v| {
                            let c = Cell::new(&match v {
                                Value::String(s) => s.to_string(),
                                Value::Null => "".into(),
                                v => v.to_string(),
                            });

                            if idx == l - 1 {
                                c.with_style(Attr::Bold)
                            } else {
                                c
                            }
                        })
                        .collect(),
                );
                table.add_row(row_strings);
            }
        }
    }
    let _ = table.printstd();
}

use actix::SystemRunner;
use actix_wamp::PubSubEndpoint;
use failure::_core::cell::RefCell;
use failure::_core::cmp::Ordering;
use failure::_core::sync::atomic::AtomicBool;
use golem_rpc_api::core::AsGolemCore;
use golem_rpc_api::terms::AsGolemTerms;
use golem_rpc_api::Net;
use prettytable::{format, format::TableFormat, Table};
use std::thread::sleep;
use std::time::Duration;

lazy_static::lazy_static! {

    pub static ref FORMAT_BASIC: TableFormat = format::FormatBuilder::new()
        .column_separator('│')
        .borders('│')
        .separators(
            &[format::LinePosition::Top],
            format::LineSeparator::new('─', '┬', '┌', '┐')
        )
        .separators(
            &[format::LinePosition::Title],
            format::LineSeparator::new('─', '┼', '├', '┤')
        )
        .separators(
            &[format::LinePosition::Bottom],
            format::LineSeparator::new('─', '┴', '└', '┘')
        )
        .padding(2, 2)
        .build();
}

pub fn format_key(s: &str, full: bool) -> String {
    if full {
        return s.to_string();
    }

    let key_size = s.len();
    if key_size < 32 {
        s.into()
    } else {
        format!("{}...{}", &s[..16], &s[(key_size - 16)..])
    }
}

static PROMPT_FLAG: AtomicBool = AtomicBool::new(false);

pub fn prompt_for_acceptance(msg: &str) -> bool {
    if PROMPT_FLAG.load(std::sync::atomic::Ordering::Relaxed) {
        return true;
    }
    eprintln!();
    promptly::prompt_default(msg, true)
}
