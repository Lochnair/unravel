// SPDX-FileCopyrightText: 2026-Present Nils Andreas Svee mailto:contact@lochnair.net (github @Lochnair)
//
// SPDX-License-Identifier: MPL-2.0

use std::{
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicU32, Ordering},
    },
    time::Duration,
};

use anyhow::Context;
use mlua::{
    Function, HookTriggers, IntoLua, Lua, LuaOptions, StdLib, Table, UserData, UserDataMethods,
    VmState,
};

use super::{Backend, Event, OPTIONAL_HOOKS, REQUIRED_HOOKS, ScriptCadence, ScriptCtx, Value};
use crate::law::Cadence;

/// The instruction hook fires every this many VM instructions...
const INSTRUCTIONS_PER_TICK: u32 = 10_000;
/// ...and a single call (or the initial load) may use this many ticks.
const MAX_TICKS: u32 = 100;

pub(super) struct LuaBackend {
    lua: Lua,
    law: Table,
    ticks: Arc<AtomicU32>,
}

impl LuaBackend {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let source = std::fs::read_to_string(path)?;

        let lua = Lua::new_with(
            StdLib::TABLE | StdLib::STRING | StdLib::MATH,
            LuaOptions::default(),
        )?;
        let globals = lua.globals();
        globals.set("load", mlua::Nil)?;
        globals.set("dofile", mlua::Nil)?;

        let ticks = Arc::new(AtomicU32::new(0));
        let counter = ticks.clone();
        lua.set_hook(
            HookTriggers::new().every_nth_instruction(INSTRUCTIONS_PER_TICK),
            move |_, _| {
                if counter.fetch_add(1, Ordering::Relaxed) >= MAX_TICKS {
                    return Err(mlua::Error::runtime("instruction limit exceeded"));
                }
                Ok(VmState::Continue)
            },
        )?;

        // The budget error is a plain Lua error, so pcall could catch it and
        // keep looping. Once the budget is spent, every protected call
        // re-raises, until the error leaves the hook.
        let counter = ticks.clone();
        let spent =
            lua.create_function(move |_, ()| Ok(counter.load(Ordering::Relaxed) > MAX_TICKS))?;
        lua.load(
            r#"
            local spent, pcall, xpcall, error = ...
            local function check(...)
                if spent() then error("instruction limit exceeded", 0) end
                return ...
            end
            _G.pcall = function(...) return check(pcall(...)) end
            _G.xpcall = function(...) return check(xpcall(...)) end
            "#,
        )
        .set_name("=sandbox")
        .call::<()>((
            spent,
            globals.get::<Function>("pcall")?,
            globals.get::<Function>("xpcall")?,
            globals.get::<Function>("error")?,
        ))?;

        let law: Table = lua
            .load(&source)
            .set_name(format!("@{}", path.display()))
            .eval()
            .context("The script must return a table")?;

        for &hook in REQUIRED_HOOKS {
            match law.get::<mlua::Value>(hook)? {
                mlua::Value::Function(_) => {}
                mlua::Value::Nil => anyhow::bail!("missing required hook {hook}"),
                other => anyhow::bail!("{hook} must be a function, not {}", other.type_name()),
            }
        }
        for &hook in OPTIONAL_HOOKS {
            match law.get::<mlua::Value>(hook)? {
                mlua::Value::Function(_) | mlua::Value::Nil => {}
                other => anyhow::bail!("{hook} must be a function, not {}", other.type_name()),
            }
        }

        Ok(Self { lua, law, ticks })
    }
}

impl Backend for LuaBackend {
    fn cadence(&self) -> anyhow::Result<ScriptCadence> {
        let table: Table = self
            .law
            .get::<Option<Table>>("cadence")?
            .context("missing cadence table")?;
        let seconds = |key: &str| -> anyhow::Result<Option<Duration>> {
            table
                .get::<Option<f64>>(key)
                .with_context(|| format!("cadence.{key} must be a number"))?
                .map(|secs| {
                    Duration::try_from_secs_f64(secs)
                        .ok()
                        .filter(|d| !d.is_zero())
                        .with_context(|| format!("cadence.{key} must be above zero, not {secs}"))
                })
                .transpose()
        };

        let probe = seconds("probe")?.context("cadence.probe is required")?;
        let traffic = seconds("traffic")?.unwrap_or(probe);
        let timer = seconds("timer")?;
        let has_on_timer = self.law.get::<Option<Function>>("on_timer")?.is_some();
        match (timer.is_some(), has_on_timer) {
            (true, false) => anyhow::bail!("cadence.timer is set but on_timer is missing"),
            (false, true) => anyhow::bail!("on_timer needs cadence.timer"),
            _ => {}
        }

        Ok(ScriptCadence {
            cadence: Cadence { probe, traffic },
            timer,
        })
    }

    fn call(&mut self, event: &Event, ctx: &ScriptCtx<'_>) -> anyhow::Result<()> {
        let Some(function) = self.law.get::<Option<Function>>(event.hook())? else {
            return Ok(());
        };

        self.ticks.store(0, Ordering::Relaxed);
        let law = self.law.clone();
        let lua = &self.lua;
        lua.scope(|scope| {
            let ctx = scope.create_userdata(LuaCtx(ctx))?;
            match event {
                Event::Init(config) => function.call::<()>((law, to_lua(lua, config)?, ctx)),
                Event::Measurement(m) => {
                    let t = lua.create_table()?;
                    t.set("reflector", m.reflector.as_str())?;
                    t.set("protocol", m.protocol.as_str())?;
                    t.set("role", m.role)?;
                    t.set("t", m.t)?;
                    t.set("loss", m.loss)?;
                    t.set("rtt_ms", m.rtt_ms)?;
                    t.set("up_ms", m.up_ms)?;
                    t.set("down_ms", m.down_ms)?;
                    t.set("one_way", m.one_way)?;
                    function.call::<()>((law, t, ctx))
                }
                Event::Traffic(s) => {
                    let t = lua.create_table()?;
                    t.set("t", s.t)?;
                    t.set("download_kbps", s.download_kbps)?;
                    t.set("upload_kbps", s.upload_kbps)?;
                    function.call::<()>((law, t, ctx))
                }
                Event::Timer(now) => function.call::<()>((law, *now, ctx)),
                Event::ClockStep => function.call::<()>((law, ctx)),
            }
        })?;
        Ok(())
    }
}

fn to_lua(lua: &Lua, value: &Value) -> mlua::Result<mlua::Value> {
    Ok(match value {
        Value::Nil => mlua::Nil,
        Value::Bool(b) => mlua::Value::Boolean(*b),
        Value::Number(n) => mlua::Value::Number(*n),
        Value::String(s) => s.as_str().into_lua(lua)?,
        Value::Array(items) => {
            let table = lua.create_table()?;
            for item in items {
                table.push(to_lua(lua, item)?)?;
            }
            mlua::Value::Table(table)
        }
        Value::Map(entries) => {
            let table = lua.create_table()?;
            for (key, item) in entries {
                table.set(key.as_str(), to_lua(lua, item)?)?;
            }
            mlua::Value::Table(table)
        }
    })
}

struct LuaCtx<'a>(&'a ScriptCtx<'a>);

impl UserData for LuaCtx<'_> {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("set_rate", |_, this, (direction, kbps): (String, f64)| {
            this.0
                .set_rate(&direction, kbps)
                .map_err(mlua::Error::external)
        });
        methods.add_method("base", |_, this, direction: String| {
            this.0.base(&direction).map_err(mlua::Error::external)
        });
        methods.add_method("min", |_, this, direction: String| {
            this.0.min(&direction).map_err(mlua::Error::external)
        });
        methods.add_method("num_reflectors", |_, this, ()| Ok(this.0.num_reflectors()));
        methods.add_method("log", |_, this, (level, message): (String, String)| {
            this.0.log(&level, &message).map_err(mlua::Error::external)
        });
        methods.add_method("store_read", |_, this, name: String| {
            this.0.store_read(&name).map_err(mlua::Error::external)
        });
        methods.add_method(
            "store_write",
            |_, this, (name, contents): (String, String)| {
                this.0
                    .store_write(&name, &contents)
                    .map_err(mlua::Error::external)
            },
        );
        methods.add_method("event", |_, this, (name, reason): (String, String)| {
            this.0.event(&name, &reason);
            Ok(())
        });
    }
}
