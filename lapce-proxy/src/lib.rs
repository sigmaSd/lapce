pub mod buffer;
pub mod dispatch;
pub mod lsp;
pub mod plugin;
pub mod terminal;
pub mod watcher;

use dispatch::{Dispatcher, NewDispatcher};

pub fn mainloop() {
    let (sender, receiver) = lapce_rpc::stdio();
    let dispatcher = Dispatcher::new(sender);
    let _ = dispatcher.mainloop(receiver);
}

pub fn new_mainloop() {
    let (core_sender, proxy_sender, proxy_receiver) = lapce_rpc::new_stdio();
    let mut dispatcher = NewDispatcher::new(core_sender, proxy_sender);
    dispatcher.mainloop(proxy_receiver);
}
