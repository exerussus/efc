//! # efc-core — файловый канал efc/1
//!
//! Программа (сервер) и её клиент обмениваются вызовами через общую папку: клиент кладёт запрос в
//! `requests/`, сервер забирает его в `active/` и пишет итог в `done/`. Стандарт — `SPEC.md` в корне репозитория.
//!
//! - [`server`] — встроить сервер в свою программу: реализовать [`server::Handler`] и вызвать [`server::Server::start`];
//! - [`client`] — вызвать команду, дождаться итога, отозвать, отменить, найти каналы;
//! - [`check`] — проверочный набор против любого сервера;
//! - [`model`], [`fsx`], [`time`] — файлы канала, атомарная запись, время.

pub mod check;
pub mod client;
pub mod fsx;
pub mod model;
pub mod server;
pub mod steps;
pub mod time;
pub mod trace;

pub use client::{CallOpts, Client, Waited};
pub use model::{exit, reason, Channel, CommandInfo, Done, Layout, Op, PathRef, Request};
pub use server::{Ctx, Handler, Outcome, Server, ServerConfig};
