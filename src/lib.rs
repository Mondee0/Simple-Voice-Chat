//! Simple Voice Chat for Weft: proximity voice with push-to-talk, groups, indicators and recording.
//!
//! Voice is captured and played through cpal streams owned by this mod (the SDK only plays
//! registered sound assets). Frames are 8-bit companded 16 kHz mono samples, relayed by the host
//! over typed `Vec<u8>` channels: `MIC` client -> server, `SPEAK` server -> listeners.

mod audio;
mod codec;
mod state;

use audio::Settings;
use parking_lot::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use weft_sdk::*;

declare_mod!(deps: ["base"], init: init);

static MIC: Channel<Vec<u8>> = Channel::named("mic");
static SPEAK: Channel<Vec<u8>> = Channel::named("speak");
static JOIN: Channel<String> = Channel::named("join_group");
static LEAVE: Channel<()> = Channel::named("leave_group");
static ROSTER_CH: Channel<String> = Channel::named("roster");
static GROUP_INFO: Channel<String> = Channel::named("group_info");
static GROUP_MEMBERS: Channel<String> = Channel::named("group_members");
static PUBLIC_GROUPS: Channel<String> = Channel::named("public_groups");
static GROUP_ERROR: Channel<String> = Channel::named("group_error");

static MUTED: Config<bool> = Config::named("muted");
static MODE: Config<u32> = Config::named("mode");
static VOLUME: Config<f32> = Config::named("volume");
static MIC_GAIN: Config<f32> = Config::named("mic_gain");
static THRESHOLD: Config<f32> = Config::named("threshold");
static RANGE: Config<f32> = Config::named("proximity_range");
static RECORD: Config<bool> = Config::named("recording");
static ECHO: Config<bool> = Config::named("hear_yourself");
static SUPPRESS: Config<bool> = Config::named("noise_suppression");
static AUTO_GAIN: Config<bool> = Config::named("auto_gain");
static AUDIO_3D: Config<u32> = Config::named("audio_3d");

static GUI_KEY: Key = Key::named("voice_chat");
static PTT: Key = Key::named("push_to_talk");
static MUTE_KEY: Key = Key::named("mute");
static MAIN_MENU: Menu = Menu::named("voice_chat");
static SETTINGS_MENU: Menu = Menu::named("voice_settings");
static CREATE_MENU: Menu = Menu::named("create_group");
static GROUP_MENU: Menu = Menu::named("my_group");
static DEVICE_MENU: Menu = Menu::named("select_device");
static ADVANCED_MENU: Menu = Menu::named("advanced_settings");
static SPEAKING_HUD: HudElement = HudElement::named("speaking");
static MIC_HUD: HudElement = HudElement::named("mic");
static GROUP_FACES: HudElement = HudElement::named("group_faces");
static UI_KEYS: Mutex<Vec<String>> = Mutex::new(Vec::new());
static GROUP_TYPE: AtomicU32 = AtomicU32::new(0);
static PREFS_LOADED: AtomicBool = AtomicBool::new(false);
static LAST_RECORD: AtomicBool = AtomicBool::new(false);

fn init(r: &mut Registry) {
    config(r);
    network(r);
    client(r);
    events(r);
}

fn config(r: &mut Registry) {
    r.config(&MUTED).bool(false);
    r.config(&MODE).choice(0, &["voice", "ptt", "always"]);
    r.config(&VOLUME).float(1.0, 0.0, 1.0).step(0.05);
    r.config(&MIC_GAIN).float(1.0, 0.0, 3.0).step(0.1);
    r.config(&THRESHOLD).float(0.01, 0.0, 0.1).step(0.005);
    r.config(&RANGE).float(32.0, 4.0, 128.0).step(2.0).server();
    r.config(&RECORD).bool(false);
    r.config(&ECHO).bool(false);
    r.config(&SUPPRESS).bool(true);
    r.config(&AUTO_GAIN).bool(true);
    r.config(&AUDIO_3D).choice(0, &["normal", "off", "reduced"]);
}

fn roster(w: &World) -> String {
    w.players()
        .into_iter()
        .map(|id| {
            let name = w.player_name(id).unwrap_or_else(|| format!("{id}"));
            let key = w.player_key(id).unwrap_or_else(|| format!("{id}"));
            format!("{id}|{name}|{key}")
        })
        .collect::<Vec<_>>()
        .join(";")
}

fn send_roster(w: &World) {
    let list = roster(w);
    w.send(Who::All, &ROSTER_CH, &list);
}

/// Tells every member of each group who else is in it, for the My Group screen.
fn send_groups(w: &World) {
    let groups: Vec<(u64, Vec<u64>)> = state::SERVER
        .lock()
        .groups
        .iter()
        .map(|(id, members)| (*id, members.iter().copied().collect()))
        .collect();
    for (_, members) in groups {
        let list = members.iter().map(u64::to_string).collect::<Vec<_>>().join(",");
        for id in members {
            w.send(id, &GROUP_MEMBERS, &list);
        }
    }
}

/// Advertises existing groups so players who are not in one can join them.
fn send_public_groups(w: &World) {
    let list = {
        let server = state::SERVER.lock();
        server
            .group_info
            .iter()
            .map(|(id, info)| {
                format!(
                    "{}|{}|{}|{}",
                    id,
                    info.name,
                    u8::from(info.password),
                    info.kind
                )
            })
            .collect::<Vec<_>>()
            .join(";")
    };
    w.send(Who::All, &PUBLIC_GROUPS, &list);
}

fn leave_group(id: u64, w: &World) {
    let mut server = state::SERVER.lock();
    if let Some(group) = server.member_of.remove(&id) {
        if let Some(members) = server.groups.get_mut(&group) {
            members.remove(&id);
            if members.is_empty() {
                server.groups.remove(&group);
                server.group_info.remove(&group);
            }
        }
    }
    drop(server);
    w.send(id, &GROUP_MEMBERS, &String::new());
}

fn network(r: &mut Registry) {
    r.packet(&MIC, |p, bytes: Vec<u8>| {
        let w = p.world();
        let id = p.player();
        let Some(entity) = w.entity(id) else { return };
        let pos = entity.pos;
        let mut server = state::SERVER.lock();
        server.seq = server.seq.wrapping_add(1);
        let seq = server.seq as u32;
        let group = *server.member_of.get(&id).unwrap_or(&0);
        drop(server);
        let packet = codec::wrap(
            seq,
            id,
            w.dimension(),
            group,
            [pos.x as f32, pos.y as f32, pos.z as f32],
            &bytes,
        );
        if group != 0 {
            w.send(Who::All, &SPEAK, &packet);
        } else {
            let range = RANGE.get().max(1.0) as f64;
            w.send(Who::Near(w.dimension(), pos, range), &SPEAK, &packet);
        }
    });

    r.packet(&JOIN, |p, text: String| {
        let w = p.world();
        let id = p.player();
        let mut parts = text.split('|');
        let name = parts.next().unwrap_or("");
        let password = parts.next().unwrap_or("");
        let kind = parts.next().unwrap_or("0").parse::<u32>().unwrap_or(0);
        if name.is_empty() {
            leave_group(id, &w);
            w.send(id, &GROUP_INFO, &"0|".to_string());
        } else {
            let group = state::group_id(name, &format!("{password}:{kind}"));
            let server = state::SERVER.lock();
            let taken = server.group_info.iter().find(|(_, info)| info.name == name).map(|(id, _)| *id);
            if taken.is_some_and(|taken| taken != group) {
                drop(server);
                w.send(id, &GROUP_ERROR, &"That group needs a different password".to_string());
                return;
            }
            drop(server);
            leave_group(id, &w);
            let mut server = state::SERVER.lock();
            server.member_of.insert(id, group);
            server.groups.entry(group).or_default().insert(id);
            server.group_info.insert(
                group,
                state::GroupInfo { name: name.to_string(), password: !password.is_empty(), kind },
            );
            drop(server);
            w.send(id, &GROUP_INFO, &format!("{group}|{name}"));
            w.send(id, &GROUP_ERROR, &String::new());
        }
        send_roster(&w);
        send_groups(&w);
        send_public_groups(&w);
    });

    r.packet(&LEAVE, |p, ()| {
        let w = p.world();
        leave_group(p.player(), &w);
        w.send(p.player(), &GROUP_INFO, &"0|".to_string());
        send_roster(&w);
        send_groups(&w);
        send_public_groups(&w);
    });

    r.client_packet(&SPEAK, |c, bytes: Vec<u8>| {
        // The host relays voice back to the speaker too. "Hear yourself" keeps that copy.
        let echo = ECHO.get();
        if let Some(engine) = audio::state() {
            engine.lock().push_network(&bytes, c.player(), echo);
        }
    });
    r.client_packet(&ROSTER_CH, |_, text: String| state::parse_roster(&text));
    r.client_packet(&GROUP_INFO, |_, text: String| {
        let (id, name) = text.split_once('|').unwrap_or((text.as_str(), ""));
        state::MY_GROUP.store(id.parse().unwrap_or(0), Ordering::Relaxed);
        *state::MY_GROUP_NAME.lock() = name.to_string();
    });
    r.client_packet(&GROUP_MEMBERS, |_, text: String| state::parse_group_members(&text));
    r.client_packet(&PUBLIC_GROUPS, |_, text: String| state::parse_public_groups(&text));
    r.client_packet(&GROUP_ERROR, |_, text: String| {
        *state::GROUP_ERROR.lock() = text;
    });
}

fn pref(i: usize) -> Option<(String, f32, bool)> {
    let key = UI_KEYS.lock().get(i).cloned()?;
    let prefs = state::PREFS.lock();
    let p = prefs.get(&key)?;
    Some((key, p.volume, p.muted))
}

fn client(r: &mut Registry) {
    r.key(&GUI_KEY, "V").on_client(|c| {
        c.menu_open(&MAIN_MENU);
    });
    r.key(&PTT, "B");
    r.key(&MUTE_KEY, "M").on_client(|_| MUTED.set(!MUTED.get()));

    r.client_frame(|c| {
        if audio::take_rebuild() {
            audio::rebuild();
        }
        let info = c.state();
        *state::LISTENER.lock() = [info.pos.x as f32, info.pos.y as f32, info.pos.z as f32];
        state::YAW.store(info.yaw.to_bits(), Ordering::Relaxed);
        state::MY_ID.store(c.player(), Ordering::Relaxed);
        state::PTT.store(c.key_held(&PTT), Ordering::Relaxed);

        {
            let mut dir = state::RUN_DIR.lock();
            if dir.is_empty() {
                *dir = c.run_dir();
            }
        }
        if !PREFS_LOADED.swap(true, Ordering::Relaxed) {
            state::load_prefs();
        }

        let settings = Settings {
            mode: MODE.get(),
            muted: MUTED.get(),
            threshold: THRESHOLD.get(),
            gain: MIC_GAIN.get(),
            volume: VOLUME.get(),
            range: RANGE.get(),
            recording: RECORD.get(),
            suppress: SUPPRESS.get(),
            auto_gain: AUTO_GAIN.get(),
            audio3d: AUDIO_3D.get(),
        };
        let recording = settings.recording;
        if let Some(engine) = audio::state() {
            engine.lock().configure(settings);
        }
        if LAST_RECORD.swap(recording, Ordering::Relaxed) && !recording {
            if let Some(engine) = audio::state() {
                engine.lock().take_recording(c.player());
            }
        }

        {
            let mut queue = state::OUTGOING.lock();
            while queue.len() > 6 {
                queue.pop_front();
            }
            for _ in 0..queue.len().min(4) {
                if let Some(bytes) = queue.pop_front() {
                    c.send(&MIC, &bytes);
                }
            }
        }
        if let Some(join) = state::JOIN_REQ.lock().take() {
            c.send(&JOIN, &join);
        }
        if state::LEAVE_REQ.swap(false, Ordering::Relaxed) {
            c.send(&LEAVE, &());
        }

        // A mic icon floats above whoever is speaking, including you.
        if info.flags & abi::client_flags::IN_WORLD != 0 {
            if state::SELF_SPEAKING.load(Ordering::Relaxed) {
                if let Some(me) = c.player_state() {
                    c.draw_text(None, me.eye().offset(0.0, 0.6, 0.0), "simple_voice_chat.mic_world", &[], 0xFFFFFFFF, 1.0);
                }
            }
            for id in state::speaking_ids() {
                if id == c.player() {
                    continue;
                }
                if let Some(entity) = c.entity(id) {
                    let eye = entity.eye();
                    let r = 0.45;
                    c.draw_box(None, [eye.x - r, eye.y - r, eye.z - r], [eye.x + r, eye.y + r, eye.z + r], 0xFFFFFFFF);
                    c.draw_text(None, eye.offset(0.0, 0.6, 0.0), "simple_voice_chat.mic_world", &[], 0xFFFFFFFF, 1.0);
                }
            }
        }
    });

    r.hud_element(&SPEAKING_HUD, hud_anchor::ABOVE_HOTBAR, 0, |ui| {
        ui.text_bind(|_, out| {
            let text = state::speaking_text();
            if !text.is_empty() {
                out.set("simple_voice_chat.speaking", &[Arg::Text(&text)]);
            } else {
                out.text("");
            }
        });
    });
    r.hud_element(&MIC_HUD, hud_anchor::BOTTOM_RIGHT, 0, |ui| {
        ui.icons("mic", 1, |_| {
            if state::SELF_SPEAKING.load(Ordering::Relaxed) { 1.0 } else { -1.0 }
        });
    });
    r.hud_element(&GROUP_FACES, hud_anchor::TOP_LEFT, 0, |ui| {
        if state::MY_GROUP.load(Ordering::Relaxed) == 0 {
            return;
        }
        let self_id = state::MY_ID.load(Ordering::Relaxed);
        let mut members = state::MY_GROUP_MEMBERS.lock().clone();
        if !members.contains(&self_id) {
            members.insert(0, self_id);
        }
        for _ in members {
            ui.panel("input", S::default().size(14.0, 14.0), |ui| {
                ui.image("simple_voice_chat:gui/head", 12.0, 12.0);
            });
        }
    })
    .rebuild_when(|_| state::GROUP_REV.load(Ordering::Relaxed));

    main_menu(r);
    settings_menu(r);
    advanced_menu(r);
    device_menu(r);
    create_menu(r);
    group_menu(r);
}

fn main_menu(r: &mut Registry) {
    r.menu(&MAIN_MENU).screen(|ui| {
        ui.panel("panel", S::column().pad(8.0).gap(6.0).w(230.0), |ui| {
            ui.text_styled(S::default().center(), "simple_voice_chat.main_title", &[]);
            ui.row(S::row().gap(8.0), |ui| {
                ui.button("simple_voice_chat.settings", S::default().w(105.0), |c| {
                    c.client().menu_open(&SETTINGS_MENU);
                });
                ui.button("simple_voice_chat.group", S::default().w(105.0), |c| {
                    if state::MY_GROUP.load(Ordering::Relaxed) == 0 {
                        c.client().menu_open(&CREATE_MENU);
                    } else {
                        c.client().menu_open(&GROUP_MENU);
                    }
                });
            });
            ui.row(S::row().gap(2.0), |ui| {
                ui.icon_button(
                    "simple_voice_chat:gui/mic_on", 10.0, 10.0,
                    S::default().tooltip("simple_voice_chat.icon_mic"),
                    |_| MUTED.set(!MUTED.get()),
                );
                ui.icon_button(
                    "core:gui/icon_sound", 10.0, 10.0,
                    S::default().tooltip("simple_voice_chat.icon_echo"),
                    |_| ECHO.set(!ECHO.get()),
                );
                ui.icon_button(
                    "base:gui/icon_settings", 10.0, 10.0,
                    S::default().tooltip("simple_voice_chat.icon_settings"),
                    |c| { c.client().menu_open(&SETTINGS_MENU); },
                );
                ui.spacer();
                ui.icon_button(
                    "base:gui/led_off", 10.0, 10.0,
                    S::default().tooltip("simple_voice_chat.icon_mute"),
                    |_| MUTED.set(!MUTED.get()),
                );
                ui.icon_button(
                    "core:gui/icon_search", 10.0, 10.0,
                    S::default().tooltip("simple_voice_chat.icon_create"),
                    |c| {
                        *state::SELECTED_GROUP.lock() = None;
                        c.client().menu_open(&CREATE_MENU);
                    },
                );
            });
        });
    });
}

fn create_menu(r: &mut Registry) {
    r.menu(&CREATE_MENU).screen(|ui| {
        ui.panel("panel", S::column().pad(8.0).gap(4.0).w(220.0), |ui| {
            ui.text_styled(S::default().w(200.0).center(), "simple_voice_chat.create_title", &[]);
            let (name, password) = {
                let mut input = state::GROUP_INPUT.lock();
                if let Some((selected, kind)) = state::SELECTED_GROUP.lock().clone() {
                    input.0 = selected;
                    GROUP_TYPE.store(kind, Ordering::Relaxed);
                }
                (input.0.clone(), input.1.clone())
            };
            ui.text("simple_voice_chat.group_name");
            ui.input(&name, 24, S::default().w(200.0), |_, s| {
                state::GROUP_INPUT.lock().0 = s.to_string();
            });
            ui.text("simple_voice_chat.password");
            ui.input(&password, 24, S::default().w(200.0), |_, s| {
                state::GROUP_INPUT.lock().1 = s.to_string();
            });
            ui.button_content(S::default().w(200.0), 0, |_, _| {
                GROUP_TYPE.store((GROUP_TYPE.load(Ordering::Relaxed) + 1) % 2, Ordering::Relaxed);
            }, |ui| {
                ui.text_bind(|_, out| {
                    let text = if GROUP_TYPE.load(Ordering::Relaxed) == 0 {
                        "Group Type: Normal"
                    } else {
                        "Group Type: Private"
                    };
                    out.text(text);
                });
            });
            ui.button("simple_voice_chat.create", S::default().w(200.0), |c| {
                let input = state::GROUP_INPUT.lock();
                *state::JOIN_REQ.lock() = Some(format!(
                    "{}|{}|{}", input.0, input.1, GROUP_TYPE.load(Ordering::Relaxed)
                ));
                drop(input);
                *state::SELECTED_GROUP.lock() = None;
                c.client().menu_open(&GROUP_MENU);
            });
        });
    });
}

fn group_menu(r: &mut Registry) {
    r.menu(&GROUP_MENU).screen(|ui| {
        ui.panel("panel", S::column().pad(8.0).gap(3.0).w(230.0), |ui| {
            ui.text_styled(S::default().w(210.0).center(), "simple_voice_chat.my_group_title", &[]);
            if state::MY_GROUP.load(Ordering::Relaxed) == 0 {
                ui.button("simple_voice_chat.create", S::default().w(200.0), |c| {
                    *state::SELECTED_GROUP.lock() = None;
                    c.client().menu_open(&CREATE_MENU);
                });
                ui.text("simple_voice_chat.other_groups");
                let groups = state::PUBLIC_GROUPS.lock().clone();
                for group in groups {
                    let g = group.clone();
                    let label = if group.password {
                        format!("{}  (password)", group.name)
                    } else {
                        group.name.clone()
                    };
                    ui.button_content(S::default().w(210.0), 0, move |c, _| {
                        *state::SELECTED_GROUP.lock() = Some((g.name.clone(), g.kind));
                        if g.password {
                            c.client().menu_open(&CREATE_MENU);
                        } else {
                            *state::JOIN_REQ.lock() = Some(format!("{}||{}", g.name, g.kind));
                        }
                    }, move |ui| {
                        ui.text_bind(move |_, out| out.text(&label));
                    });
                }
                ui.text_bind(|_, out| {
                    let error = state::GROUP_ERROR.lock().clone();
                    if !error.is_empty() {
                        out.text(&error);
                    }
                });
            } else {
                let self_id = state::MY_ID.load(Ordering::Relaxed);
                let mut members: Vec<u64> = state::MY_GROUP_MEMBERS.lock().clone();
                if !members.contains(&self_id) {
                    members.insert(0, self_id);
                }
                for id in members {
                    ui.row(S::row().gap(4.0).w(210.0).h(20.0), |ui| {
                        ui.image("simple_voice_chat:gui/head", 12.0, 12.0);
                        let name = if id == self_id {
                            settings::NAME.get()
                        } else {
                            state::player_name(id)
                        };
                        ui.text_bind(move |_, out| out.text(&name));
                    });
                }
            }
            ui.row(S::row().gap(3.0), |ui| {
                ui.icon_button(
                    "simple_voice_chat:gui/mic_on", 10.0, 10.0,
                    S::default().tooltip("simple_voice_chat.icon_mic"),
                    |_| MUTED.set(!MUTED.get()),
                );
                ui.icon_button(
                    "core:gui/icon_sound", 10.0, 10.0,
                    S::default().tooltip("simple_voice_chat.icon_echo"),
                    |_| ECHO.set(!ECHO.get()),
                );
                ui.icon_button(
                    "base:gui/icon_settings", 10.0, 10.0,
                    S::default().tooltip("simple_voice_chat.icon_settings"),
                    |c| { c.client().menu_open(&SETTINGS_MENU); },
                );
                ui.spacer();
                ui.icon_button(
                    "simple_voice_chat:gui/leave", 12.0, 12.0,
                    S::default().tooltip("simple_voice_chat.leave_group"),
                    |_| state::LEAVE_REQ.store(true, Ordering::Relaxed),
                );
            });
        });
    });
}

fn settings_menu(r: &mut Registry) {
    r.menu(&SETTINGS_MENU).screen(|ui| {
        ui.panel("panel", S::column().pad(8.0).gap(4.0).w(240.0), |ui| {
            ui.text_styled(S::default().w(220.0).center(), "simple_voice_chat.settings_title", &[]);
            ui.panel("input", S::default().h(18.0), |ui| {
                ui.text_bind(|_, out| {
                    out.set("simple_voice_chat.volume_percent", &[Arg::Int((VOLUME.get() * 100.0) as i64)]);
                });
                ui.slider(S::default().h(6.0), |_| VOLUME.get(), |_, value| VOLUME.set(value));
            });
            ui.row(S::row().gap(2.0), |ui| {
                ui.button_content(S::default().w(105.0), 0, |_, _| AUTO_GAIN.set(!AUTO_GAIN.get()), |ui| {
                    ui.text_bind(|_, out| {
                        if AUTO_GAIN.get() {
                            out.set("simple_voice_chat.gain_auto", &[]);
                        } else {
                            out.set("simple_voice_chat.gain_manual", &[]);
                        }
                    });
                });
                ui.panel("input", S::default().h(24.0).grow(), |ui| {
                    ui.text_bind(|_, out| {
                        let db = 20.0 * (MIC_GAIN.get() as f64).max(0.01).log10();
                        out.set("simple_voice_chat.mic_gain_db", &[Arg::Float(db, 0)]);
                    });
                    ui.slider(S::default().h(8.0), |_| MIC_GAIN.get() / 3.0, |_, v| MIC_GAIN.set(v * 3.0));
                });
            });
            ui.button_content(S::default().w(220.0), 0, |_, _| SUPPRESS.set(!SUPPRESS.get()), |ui| {
                ui.text_bind(|_, out| {
                    if SUPPRESS.get() {
                        out.set("simple_voice_chat.suppress_on", &[]);
                    } else {
                        out.set("simple_voice_chat.suppress_off", &[]);
                    }
                });
            });
            ui.row(S::row().gap(2.0), |ui| {
                ui.icon_button("simple_voice_chat:gui/mic_on", 12.0, 12.0, S::default(), |_| {
                    MUTED.set(!MUTED.get());
                });
                ui.button_content(S::default().grow(), 0, |_, _| MODE.set((MODE.get() + 1) % 3), |ui| {
                    ui.text_bind(|_, out| {
                        let text = match MODE.get() {
                            0 => "Activation Method: Voice",
                            1 => "Activation Method: Push to talk",
                            _ => "Activation Method: Always on",
                        };
                        out.text(text);
                    });
                });
            });
            ui.button_content(S::default().w(220.0), 0, |_, _| {
                let next = match THRESHOLD.get() {
                    t if t < 0.02 => 0.03,
                    t if t < 0.05 => 0.08,
                    _ => 0.01,
                };
                THRESHOLD.set(next);
            }, |ui| {
                ui.text_bind(|_, out| {
                    let text = match THRESHOLD.get() {
                        t if t < 0.02 => "Voice Activation: Auto",
                        t if t < 0.05 => "Voice Activation: Low",
                        _ => "Voice Activation: High",
                    };
                    out.text(text);
                });
            });
            ui.progress_bind("voice_meter", S::default().h(20.0).w(220.0), |_| {
                f32::from_bits(state::MIC_LEVEL.load(Ordering::Relaxed))
            });
            ui.button_content(S::default().w(220.0), 0, |_, _| AUDIO_3D.set((AUDIO_3D.get() + 1) % 3), |ui| {
                ui.text_bind(|_, out| {
                    let text = match AUDIO_3D.get() {
                        0 => "3D Audio: Normal",
                        1 => "3D Audio: Off",
                        _ => "3D Audio: Reduced",
                    };
                    out.text(text);
                });
            });
            ui.row(S::row().gap(2.0), |ui| {
                ui.button("simple_voice_chat.select_mic", S::default().w(108.0), |c| {
                    c.client().menu_open(&DEVICE_MENU);
                });
                ui.button("simple_voice_chat.select_speaker", S::default().w(108.0), |c| {
                    c.client().menu_open(&DEVICE_MENU);
                });
            });
            ui.button("simple_voice_chat.advanced", S::default().w(220.0), |c| {
                c.client().menu_open(&ADVANCED_MENU);
            });
        });
    });
}

fn device_menu(r: &mut Registry) {
    r.menu(&DEVICE_MENU).screen(|ui| {
        ui.panel("panel", S::column().pad(8.0).gap(3.0).w(240.0).h(320.0).scroll(), |ui| {
            ui.text("simple_voice_chat.microphones");
            for name in audio::device_names(true) {
                let label = name.clone();
                ui.button_args("simple_voice_chat.device_button", &[Arg::Text(&label)], S::default().w(220.0), move |_| {
                    audio::select(true, &name);
                });
            }
            ui.text("simple_voice_chat.speakers");
            for name in audio::device_names(false) {
                let label = name.clone();
                ui.button_args("simple_voice_chat.device_button", &[Arg::Text(&label)], S::default().w(220.0), move |_| {
                    audio::select(false, &name);
                });
            }
        });
    });
}

fn advanced_menu(r: &mut Registry) {
    r.menu(&ADVANCED_MENU).screen(|ui| {
        ui.panel("panel", S::column().pad(8.0).gap(4.0).h(320.0).scroll(), |ui| {
            ui.text("simple_voice_chat.settings_title");
            ui.button("simple_voice_chat.mute", S::default(), |_| MUTED.set(!MUTED.get()));
            ui.button("simple_voice_chat.echo", S::default(), |_| ECHO.set(!ECHO.get()));
            ui.button("simple_voice_chat.mode", S::default(), |_| MODE.set((MODE.get() + 1) % 3));
            ui.text_bind(|_, out| {
                out.set("simple_voice_chat.mode_value", &[Arg::Int(MODE.get() as i64)]);
            });
            ui.text("simple_voice_chat.receive_volume");
            ui.slider(S::default(), |_| VOLUME.get(), |_, v| VOLUME.set(v));
            ui.text("simple_voice_chat.mic_gain");
            ui.slider(S::default(), |_| MIC_GAIN.get() / 3.0, |_, v| MIC_GAIN.set(v * 3.0));
            ui.text("simple_voice_chat.threshold");
            ui.slider(S::default(), |_| THRESHOLD.get() / 0.1, |_, v| THRESHOLD.set(v * 0.1));
            ui.text("simple_voice_chat.range");
            ui.slider(S::default(), |_| (RANGE.get() - 4.0) / 124.0, |_, v| RANGE.set(4.0 + v * 124.0));

            ui.text("simple_voice_chat.devices_title");
            ui.text_bind(|_, out| out.text(&format!("Input: {}", audio::device_label(true))));
            ui.button("simple_voice_chat.next_input", S::default(), |_| audio::select_next(true));
            ui.text_bind(|_, out| out.text(&format!("Output: {}", audio::device_label(false))));
            ui.button("simple_voice_chat.next_output", S::default(), |_| audio::select_next(false));

            ui.text("simple_voice_chat.players_title");
            let keys: Vec<String> = {
                let prefs = state::PREFS.lock();
                let mut keys: Vec<String> = prefs.keys().cloned().collect();
                keys.sort();
                keys
            };
            *UI_KEYS.lock() = keys.clone();
            for (i, key) in keys.iter().enumerate() {
                let label = state::PREFS.lock().get(key).map_or_else(|| key.clone(), |p| p.name.clone());
                ui.text_bind(move |_, out| {
                    if let Some((_, volume, muted)) = pref(i) {
                        let state = if muted { "muted" } else { "on" };
                        out.text(&format!("{label} - volume {volume:.1} - {state}"));
                    }
                });
                ui.button_data("simple_voice_chat.player_mute", &[], S::default(), i as u32, |_, i| {
                    if let Some((key, _, _)) = pref(i as usize) {
                        let changed = {
                            let mut prefs = state::PREFS.lock();
                            prefs.get_mut(&key).map_or(false, |p| {
                                p.muted = !p.muted;
                                true
                            })
                        };
                        if changed {
                            state::save_prefs();
                        }
                    }
                });
                ui.slider(
                    S::default(),
                    move |_| pref(i).map_or(0.0, |(_, v, _)| v / 2.0),
                    move |_, value| {
                        if let Some((key, _, _)) = pref(i) {
                            let changed = {
                                let mut prefs = state::PREFS.lock();
                                prefs.get_mut(&key).map_or(false, |p| {
                                    p.volume = value * 2.0;
                                    true
                                })
                            };
                            if changed {
                                state::save_prefs();
                            }
                        }
                    },
                );
            }
        });
    });

    r.screen_part(&screens::SETTINGS, "buttons", 0, |ui| {
        ui.button("simple_voice_chat.settings_button", S::default(), |c| {
            c.client().menu_open(&MAIN_MENU);
        });
    });
}

fn events(r: &mut Registry) {
    r.on_event(&PLAYER_JOINED, |batch| {
        let w = batch.world();
        send_roster(&w);
        send_groups(&w);
        send_public_groups(&w);
    });
    r.on_event(&ENTITY_REMOVED, |batch| {
        let w = batch.world();
        let mut changed = false;
        for removed in batch.iter() {
            if removed.reason == abi::removal::LEFT {
                leave_group(removed.entity, &w);
                changed = true;
            }
        }
        if changed {
            send_roster(&w);
            send_groups(&w);
            send_public_groups(&w);
        }
    });
}
