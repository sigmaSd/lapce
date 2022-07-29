pub mod catalog;
pub mod lsp;
pub mod psp;
pub mod wasi;

use anyhow::{anyhow, Result};
use crossbeam_channel::{Receiver, Sender};
use dyn_clone::DynClone;
use home::home_dir;
use jsonrpc_lite::Id;
use lapce_rpc::core::CoreRpcHandler;
use lapce_rpc::counter::Counter;
use lapce_rpc::plugin::{PluginDescription, PluginId};
use lapce_rpc::proxy::ProxyRpcHandler;
use lapce_rpc::{RequestId, RpcError, RpcMessage};
use lsp_types::notification::{DidOpenTextDocument, Notification};
use lsp_types::request::{Completion, Request};
use lsp_types::{
    CompletionParams, CompletionResponse, DidOpenTextDocumentParams,
    PartialResultParams, Position, TextDocumentIdentifier, TextDocumentItem,
    TextDocumentPositionParams, Url, VersionedTextDocumentIdentifier,
    WorkDoneProgressParams,
};
use parking_lot::Mutex;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use toml;
use wasmer::Store;
use wasmer::WasmerEnv;
use wasmer_wasi::WasiEnv;
use xi_rope::{Rope, RopeDelta};

use crate::dispatch::Dispatcher;

use self::catalog::NewPluginCatalog;
use self::psp::{ClonableCallback, PluginServerRpcHandler};

pub type PluginName = String;

pub enum PluginCatalogRpc {
    ServerRequest {
        method: &'static str,
        params: Value,
        f: Box<dyn ClonableCallback>,
    },
    ServerNotification {
        method: &'static str,
        params: Value,
    },
    DidChangeTextDocument {
        document: VersionedTextDocumentIdentifier,
        delta: RopeDelta,
        text: Rope,
        new_text: Rope,
    },
    Handler(PluginCatalogNotification),
}

pub enum PluginCatalogNotification {
    PluginServerLoaded(PluginServerRpcHandler),
}

#[derive(WasmerEnv, Clone)]
pub(crate) struct PluginEnv {
    wasi_env: WasiEnv,
    desc: PluginDescription,
    dispatcher: Dispatcher,
}

#[derive(Clone)]
pub(crate) struct Plugin {
    instance: wasmer::Instance,
    env: PluginEnv,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "kebab-case")]
struct PluginConfig {
    disabled: Vec<String>,
}

#[derive(Clone)]
pub struct PluginCatalogRpcHandler {
    core_rpc: CoreRpcHandler,
    proxy_rpc: ProxyRpcHandler,
    plugin_tx: Sender<PluginCatalogRpc>,
    plugin_rx: Receiver<PluginCatalogRpc>,
    id: Arc<AtomicU64>,
    pending: Arc<Mutex<HashMap<u64, Sender<Result<Value, RpcError>>>>>,
}

impl PluginCatalogRpcHandler {
    pub fn new(core_rpc: CoreRpcHandler, proxy_rpc: ProxyRpcHandler) -> Self {
        let (plugin_tx, plugin_rx) = crossbeam_channel::unbounded();
        Self {
            core_rpc,
            proxy_rpc,
            plugin_tx,
            plugin_rx,
            id: Arc::new(AtomicU64::new(0)),
            pending: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    fn handle_response(&self, id: RequestId, result: Result<Value, RpcError>) {
        if let Some(chan) = { self.pending.lock().remove(&id) } {
            chan.send(result);
        }
    }

    pub fn mainloop(&self, plugin: &mut NewPluginCatalog) {
        for msg in &self.plugin_rx {
            match msg {
                PluginCatalogRpc::ServerRequest { method, params, f } => {
                    plugin.handle_server_request(method, params, f);
                }
                PluginCatalogRpc::ServerNotification { method, params } => {
                    plugin.handle_server_notification(method, params);
                }
                PluginCatalogRpc::Handler(notification) => {
                    plugin.handle_notification(notification);
                }
                PluginCatalogRpc::DidChangeTextDocument {
                    document,
                    delta,
                    text,
                    new_text,
                } => {
                    plugin.handle_did_change_text_document(
                        document, delta, text, new_text,
                    );
                }
            }
        }
    }

    fn catalog_notification(&self, notification: PluginCatalogNotification) {
        let _ = self.plugin_tx.send(PluginCatalogRpc::Handler(notification));
    }

    fn server_notification<P: Serialize>(&self, method: &'static str, params: P) {
        let params = serde_json::to_value(params).unwrap();
        let rpc = PluginCatalogRpc::ServerNotification { method, params };
        let _ = self.plugin_tx.send(rpc);
    }

    fn send_request<P: Serialize>(
        &self,
        method: &'static str,
        params: P,
        f: impl FnOnce(Result<Value, RpcError>) + Send + DynClone + 'static,
    ) {
        let params = serde_json::to_value(params).unwrap();
        let rpc = PluginCatalogRpc::ServerRequest {
            method,
            params,
            f: Box::new(f),
        };
        let _ = self.plugin_tx.send(rpc);
    }

    pub fn did_change_text_document(
        &self,
        path: &Path,
        rev: u64,
        delta: RopeDelta,
        text: Rope,
        new_text: Rope,
    ) {
        let document = VersionedTextDocumentIdentifier::new(
            Url::from_file_path(path).unwrap(),
            rev as i32,
        );
        let _ = self
            .plugin_tx
            .send(PluginCatalogRpc::DidChangeTextDocument {
                document,
                delta,
                text,
                new_text,
            });
    }

    pub fn completion(
        &self,
        request_id: usize,
        path: &Path,
        input: String,
        position: Position,
    ) {
        eprintln!("send completion {input} {position:?}");
        let uri = Url::from_file_path(path).unwrap();
        let method = Completion::METHOD;
        let params = CompletionParams {
            text_document_position: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri },
                position,
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
            context: None,
        };

        let core_rpc = self.core_rpc.clone();
        self.send_request(method, params, move |result| {
            if let Ok(value) = result {
                if let Ok(resp) = serde_json::from_value::<CompletionResponse>(value)
                {
                    core_rpc.completion_response(request_id, input, resp);
                }
            }
        });
    }

    pub fn document_did_open(
        &self,
        path: &Path,
        language_id: String,
        version: i32,
        text: String,
    ) {
        let method = DidOpenTextDocument::METHOD;
        let params = DidOpenTextDocumentParams {
            text_document: TextDocumentItem::new(
                Url::from_file_path(path).unwrap(),
                language_id,
                version,
                text,
            ),
        };
        self.server_notification(method, params);
    }

    pub fn plugin_server_loaded(&self, plugin: PluginServerRpcHandler) {
        self.catalog_notification(PluginCatalogNotification::PluginServerLoaded(
            plugin,
        ));
    }
}

pub struct PluginCatalog {
    id_counter: Counter,
    pub items: HashMap<PluginName, PluginDescription>,
    plugins: HashMap<PluginName, Plugin>,
    pub disabled: HashMap<PluginName, PluginDescription>,
    store: Store,
    senders: HashMap<PluginName, Sender<PluginTransmissionMessage>>,
}

enum PluginTransmissionMessage {
    Initialize,
    Stop,
}

impl PluginCatalog {
    pub fn new() -> PluginCatalog {
        PluginCatalog {
            id_counter: Counter::new(),
            items: HashMap::new(),
            plugins: HashMap::new(),
            disabled: HashMap::new(),
            store: Store::default(),
            senders: HashMap::new(),
        }
    }

    pub fn stop(&mut self) {
        self.items.clear();
        self.plugins.clear();
    }

    pub fn reload(&mut self) {
        self.items.clear();
        self.plugins.clear();
        self.disabled.clear();
        // let _ = self.load();
    }

    // pub fn load(&mut self) -> Result<()> {
    // let all_plugins = find_all_plugins();
    // for plugin_path in &all_plugins {
    //     match load_plugin(plugin_path) {
    //         Err(_e) => (),
    //         Ok(plugin) => {
    //             self.items.insert(plugin.name.clone(), plugin.clone());
    //         }
    //     }
    // }
    // let home = home_dir().unwrap();
    // let path = home.join(".lapce").join("config").join("plugins.toml");
    // let mut file = fs::File::open(path)?;
    // let mut content = String::new();
    // file.read_to_string(&mut content)?;
    // let plugin_config: PluginConfig = toml::from_str(&content)?;
    // let mut disabled = HashMap::new();
    // for plugin_name in plugin_config.disabled.iter() {
    //     if let Some(plugin) = self.items.get(plugin_name) {
    //         disabled.insert(plugin_name.clone(), plugin.clone());
    //     }
    // }
    // self.disabled = disabled;
    // Ok(())
    // }

    // pub fn install_plugin(
    //     &mut self,
    //     dispatcher: Dispatcher,
    //     plugin: PluginDescription,
    // ) -> Result<()> {
    //     let home = home_dir().unwrap();
    //     let path = home.join(".lapce").join("plugins").join(&plugin.name);
    //     let _ = fs::remove_dir_all(&path);

    //     fs::create_dir_all(&path)?;

    //     {
    //         let mut file = fs::OpenOptions::new()
    //             .create(true)
    //             .truncate(true)
    //             .write(true)
    //             .open(path.join("plugin.toml"))?;
    //         file.write_all(&toml::to_vec(&plugin)?)?;
    //     }

    //     let mut plugin = plugin;
    //     if let Some(wasm) = plugin.wasm.clone() {
    //         {
    //             let url = format!(
    //                 "https://raw.githubusercontent.com/{}/master/{}",
    //                 plugin.repository, wasm
    //             );
    //             let mut resp = reqwest::blocking::get(url)?;
    //             let mut file = fs::OpenOptions::new()
    //                 .create(true)
    //                 .truncate(true)
    //                 .write(true)
    //                 .open(path.join(&wasm))?;
    //             std::io::copy(&mut resp, &mut file)?;
    //         }

    //         plugin.dir = Some(path.clone());
    //         plugin.wasm = Some(
    //             path.join(&wasm)
    //                 .to_str()
    //                 .ok_or_else(|| anyhow!("path can't to string"))?
    //                 .to_string(),
    //         );

    //         if let Ok((p, tx)) = self.start_plugin(dispatcher, plugin.clone()) {
    //             self.plugins.insert(plugin.name.clone(), p);
    //             self.senders.insert(plugin.name.clone(), tx);
    //         }
    //     }
    //     if let Some(themes) = plugin.themes.as_ref() {
    //         for theme in themes {
    //             {
    //                 let url = format!(
    //                     "https://raw.githubusercontent.com/{}/master/{}",
    //                     plugin.repository, theme
    //                 );
    //                 let mut resp = reqwest::blocking::get(url)?;
    //                 let mut file = fs::OpenOptions::new()
    //                     .create(true)
    //                     .truncate(true)
    //                     .write(true)
    //                     .open(path.join(theme))?;
    //                 std::io::copy(&mut resp, &mut file)?;
    //             }
    //         }
    //     }
    //     self.items.insert(plugin.name.clone(), plugin);
    //     Ok(())
    // }

    // pub fn remove_plugin(
    //     &mut self,
    //     dispatcher: Dispatcher,
    //     plugin: PluginDescription,
    // ) -> Result<()> {
    //     self.disable_plugin(dispatcher, plugin.clone())?;
    //     let home = home_dir().unwrap();
    //     let path = home.join(".lapce").join("plugins").join(&plugin.name);
    //     fs::remove_dir_all(&path)?;

    //     let _ = self.items.remove(&plugin.name);
    //     let _ = self.plugins.remove(&plugin.name);
    //     let _ = self.disabled.remove(&plugin.name);
    //     Ok(())
    // }

    // pub fn start_all(&mut self, dispatcher: Dispatcher) {
    //     for (_, plugin) in self.items.clone().iter() {
    //         if !self.disabled.contains_key(&plugin.name) {
    //             if let Ok((p, _tx)) =
    //                 self.start_plugin(dispatcher.clone(), plugin.clone())
    //             {
    //                 self.plugins.insert(plugin.name.clone(), p);
    //             }
    //         }
    //     }
    // }

    // pub fn disable_plugin(
    //     &mut self,
    //     _dispatcher: Dispatcher,
    //     plugin_desc: PluginDescription,
    // ) -> Result<()> {
    //     let plugin_tx = self.senders.get(&plugin_desc.name);
    //     if let Some(tx) = plugin_tx {
    //         let local_tx = tx.clone();
    //         thread::spawn(move || {
    //             let _ = local_tx.send(PluginTransmissionMessage::Stop);
    //         });
    //     }
    //     self.senders.remove(&plugin_desc.name);
    //     let plugin = plugin_desc.clone();
    //     self.disabled.insert(plugin_desc.name.clone(), plugin);
    //     let disabled_plugin_list =
    //         self.disabled.clone().into_keys().collect::<Vec<String>>();
    //     let plugin_config = PluginConfig {
    //         disabled: disabled_plugin_list,
    //     };
    //     let home = home_dir().unwrap();
    //     let path = home.join(".lapce").join("config");
    //     fs::create_dir_all(&path)?;
    //     {
    //         let mut file = fs::OpenOptions::new()
    //             .create(true)
    //             .truncate(true)
    //             .write(true)
    //             .open(path.join("plugins.toml"))?;
    //         file.write_all(&toml::to_vec(&plugin_config)?)?;
    //     }

    //     Ok(())
    // }

    // pub fn enable_plugin(
    //     &mut self,
    //     dispatcher: Dispatcher,
    //     plugin_desc: PluginDescription,
    // ) -> Result<()> {
    //     let mut plugin = plugin_desc.clone();
    //     let home = home_dir().unwrap();
    //     let path = home.join(".lapce").join("plugins").join(&plugin.name);
    //     plugin.dir = Some(path.clone());
    //     if let Some(wasm) = plugin.wasm {
    //         plugin.wasm = Some(
    //             path.join(&wasm)
    //                 .to_str()
    //                 .ok_or_else(|| anyhow!("path can't to string"))?
    //                 .to_string(),
    //         );
    //         self.start_plugin(dispatcher, plugin.clone())?;
    //         self.disabled.remove(&plugin_desc.name);
    //         let config_path = home.join(".lapce").join("config");
    //         let disabled_plugin_list =
    //             self.disabled.clone().into_keys().collect::<Vec<String>>();
    //         let plugin_config = PluginConfig {
    //             disabled: disabled_plugin_list,
    //         };
    //         {
    //             let mut file = fs::OpenOptions::new()
    //                 .create(true)
    //                 .truncate(true)
    //                 .write(true)
    //                 .open(config_path.join("plugins.toml"))?;
    //             file.write_all(&toml::to_vec(&plugin_config)?)?;
    //         }
    //         Ok(())
    //     } else {
    //         Err(anyhow!("no wasm in plugin"))
    //     }
    // }

    pub fn next_plugin_id(&mut self) -> PluginId {
        PluginId(self.id_counter.next())
    }
}

impl Default for PluginCatalog {
    fn default() -> Self {
        Self::new()
    }
}

// pub(crate) fn lapce_exports(store: &Store, plugin_env: &PluginEnv) -> ImportObject {
//     macro_rules! lapce_export {
//         ($($host_function:ident),+ $(,)?) => {
//             wasmer::imports! {
//                 "lapce" => {
//                     $(stringify!($host_function) =>
//                         wasmer::Function::new_native_with_env(store, plugin_env.clone(), $host_function),)+
//                 }
//             }
//         }
//     }

//     lapce_export! {
//         host_handle_notification,
//     }
// }

#[derive(Serialize, Deserialize, Debug)]
#[serde(rename_all = "snake_case")]
#[serde(tag = "method", content = "params")]
pub enum PluginNotification {
    StartLspServer {
        exec_path: String,
        language_id: String,
        options: Option<Value>,
        system_lsp: Option<bool>,
    },
    DownloadFile {
        url: String,
        path: PathBuf,
    },
    LockFile {
        path: PathBuf,
    },
    MakeFileExecutable {
        path: PathBuf,
    },
}

fn number_from_id(id: &Id) -> u64 {
    match *id {
        Id::Num(n) => n as u64,
        Id::Str(ref s) => s
            .parse::<u64>()
            .expect("failed to convert string id to u64"),
        _ => panic!("unexpected value for id: None"),
    }
}

// fn host_handle_notification(plugin_env: &PluginEnv) {
//     let notification: Result<PluginNotification> =
//         wasi_read_object(&plugin_env.wasi_env);
//     if let Ok(notification) = notification {
//         match notification {
//             PluginNotification::StartLspServer {
//                 exec_path,
//                 language_id,
//                 options,
//                 system_lsp,
//             } => {
//                 let exec_path = if system_lsp.unwrap_or(false) {
//                     // System LSP should be handled by PATH during
//                     // process creation, so we forbid anything that
//                     // is not just an executable name
//                     match PathBuf::from(&exec_path).file_name() {
//                         Some(v) => v.to_str().unwrap().to_string(),
//                         None => return,
//                     }
//                 } else {
//                     plugin_env
//                         .desc
//                         .dir
//                         .clone()
//                         .unwrap()
//                         .join(&exec_path)
//                         .to_str()
//                         .unwrap()
//                         .to_string()
//                 };
//                 plugin_env.dispatcher.lsp.lock().start_server(
//                     &exec_path,
//                     &language_id,
//                     options,
//                 );
//             }
//             PluginNotification::DownloadFile { url, path } => {
//                 let mut resp = reqwest::blocking::get(url).expect("request failed");
//                 let mut out = fs::File::create(
//                     plugin_env.desc.dir.clone().unwrap().join(path),
//                 )
//                 .expect("failed to create file");
//                 std::io::copy(&mut resp, &mut out).expect("failed to copy content");
//             }
//             PluginNotification::LockFile { path } => {
//                 let path = plugin_env.desc.dir.clone().unwrap().join(path);
//                 let mut n = 0;
//                 loop {
//                     if let Ok(_file) = fs::OpenOptions::new()
//                         .write(true)
//                         .create_new(true)
//                         .open(&path)
//                     {
//                         return;
//                     }
//                     if n > 10 {
//                         return;
//                     }
//                     n += 1;
//                     let mut hotwatch =
//                         Hotwatch::new().expect("hotwatch failed to initialize!");
//                     let (tx, rx) = crossbeam_channel::bounded(1);
//                     let _ = hotwatch.watch(&path, move |_event| {
//                         #[allow(deprecated)]
//                         let _ = tx.send(0);
//                     });
//                     let _ = rx.recv_timeout(Duration::from_secs(10));
//                 }
//             }
//             PluginNotification::MakeFileExecutable { path } => {
//                 let _ = Command::new("chmod")
//                     .arg("+x")
//                     .arg(&plugin_env.desc.dir.clone().unwrap().join(path))
//                     .output();
//             }
//         }
//     }
// }

pub fn wasi_read_string(wasi_env: &WasiEnv) -> Result<String> {
    let mut state = wasi_env.state();
    let wasi_file = state
        .fs
        .stdout_mut()?
        .as_mut()
        .ok_or_else(|| anyhow!("can't get stdout"))?;
    let mut buf = String::new();
    wasi_file.read_to_string(&mut buf)?;
    Ok(buf)
}

pub fn wasi_read_object<T: DeserializeOwned>(wasi_env: &WasiEnv) -> Result<T> {
    let json = wasi_read_string(wasi_env)?;
    Ok(serde_json::from_str(&json)?)
}

pub fn wasi_write_string(wasi_env: &WasiEnv, buf: &str) {
    let mut state = wasi_env.state();
    let wasi_file = state.fs.stdin_mut().unwrap().as_mut().unwrap();
    writeln!(wasi_file, "{}\r", buf).unwrap();
}

pub fn wasi_write_object(wasi_env: &WasiEnv, object: &(impl Serialize + ?Sized)) {
    wasi_write_string(wasi_env, &serde_json::to_string(&object).unwrap());
}

pub struct PluginHandler {}

fn load_plugin(path: &Path) -> Result<PluginDescription> {
    let mut file = fs::File::open(&path)?;
    let mut contents = String::new();
    file.read_to_string(&mut contents)?;
    let mut plugin: PluginDescription = toml::from_str(&contents)?;
    plugin.dir = Some(path.parent().unwrap().canonicalize()?);
    plugin.wasm = plugin.wasm.as_ref().and_then(|wasm| {
        Some(
            path.parent()?
                .join(wasm)
                .canonicalize()
                .ok()?
                .to_str()?
                .to_string(),
        )
    });
    plugin.themes = plugin.themes.as_ref().map(|themes| {
        themes
            .iter()
            .filter_map(|theme| {
                Some(
                    path.parent()?
                        .join(theme)
                        .canonicalize()
                        .ok()?
                        .to_str()?
                        .to_string(),
                )
            })
            .collect()
    });
    Ok(plugin)
}
