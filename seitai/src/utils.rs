use std::{
    borrow::Cow,
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Context as _, Result};
use futures::lock::Mutex;
use hashbrown::HashMap;
use serenity::{
    all::{GuildId, User, UserId},
    builder::{CreateInteractionResponse, CreateInteractionResponseMessage},
    client::Context,
    model::{application::CommandInteraction, guild::Guild},
    utils::{content_safe, ContentSafeOptions},
};
use songbird::Songbird;
use soundboard::sound::SoundId;
use voicevox::Voicevox;

use crate::{regex::{self, SOUNDMOJI}, VoicevoxClient};

pub(crate) async fn get_manager(context: &Context) -> Result<Arc<Songbird>> {
    songbird::get(context)
        .await
        .context("failed to get songbird voice client: it placed in at initialisation")
}

pub(crate) fn get_guild(context: &Context, interaction: &CommandInteraction) -> Option<Guild> {
    let guild_id = interaction.guild_id?;
    guild_id.to_guild_cached(&context.cache).map(|guild| guild.to_owned())
}

pub(crate) fn parse_soundmoji(value: impl AsRef<str>) -> Result<(Option<SoundId>, Option<GuildId>)> {
    let value = value.as_ref();

    match SOUNDMOJI.captures(value) {
        Some(caps) => {
            let sound_id = caps.name("sound_id").map(|m| m.as_str().parse::<u64>()).transpose()?;
            let guild_id = caps.name("guild_id").map(|m| m.as_str().parse::<u64>()).transpose()?;

            Ok((sound_id.map(SoundId::new), guild_id.map(GuildId::new)))
        },
        None => Ok((None, None)),
    }
}

pub(crate) async fn respond(
    context: &Context,
    interaction: &CommandInteraction,
    message: &CreateInteractionResponseMessage,
) -> Result<()> {
    let builder = CreateInteractionResponse::Message(message.clone());
    interaction
        .create_response(&context.http, builder)
        .await
        .with_context(|| format!("failed to create interaction response with message: {message:?}"))?;

    Ok(())
}

pub(crate) fn normalize<'a>(context: &Context, guild_id: &GuildId, users: &[User], text: &'a str) -> Cow<'a, str> {
    match regex::MENTION_CHANNEL.is_match(text) {
        true => {
            let content_safe_options = ContentSafeOptions::new()
                .clean_role(true)
                .clean_user(true)
                .clean_channel(true)
                .show_discriminator(false)
                .display_as_member_from(guild_id)
                .clean_here(false)
                .clean_everyone(false);

            let normalized = content_safe(&context.cache, text, &content_safe_options, users);
            Cow::Owned(normalized)
        },
        false => Cow::Borrowed(text),
    }
}

pub(crate) async fn get_voicevox(context: &Context) -> Option<Arc<Mutex<Voicevox>>> {
    let data = context.data.read().await;
    data.get::<VoicevoxClient>().cloned()
}

#[derive(Clone)]
struct UserState {
    messages: Vec<Instant>,
    violation_count: usize,
    cooldown_until: Option<Instant>,
}

pub struct RateLimiter {
    // ユーザーごとの状態を保持
    users: Mutex<HashMap<UserId, UserState>>,
    // 制限時間内に許可するメッセージ数
    max_messages: usize,
    // 制限を判定する時間枠
    time_window: Duration,
    // 基本のクールダウン時間
    base_cooldown: Duration,
    // クールダウンの最大時間
    max_cooldown: Duration,
    // 違反回数に応じたクールダウン時間の乗数
    cooldown_multiplier: f32,
    // 違反カウントがリセットされるまでの時間
    violation_reset_time: Duration,
}

impl RateLimiter {
    pub fn new(
        max_messages: usize,
        time_window_secs: u64,
        base_cooldown_secs: u64,
        max_cooldown_secs: u64,
        cooldown_multiplier: f32,
        violation_reset_hours: u64,
    ) -> Self {
        Self {
            users: Mutex::new(HashMap::new()),
            max_messages,
            time_window: Duration::from_secs(time_window_secs),
            base_cooldown: Duration::from_secs(base_cooldown_secs),
            max_cooldown: Duration::from_secs(max_cooldown_secs),
            cooldown_multiplier,
            violation_reset_time: Duration::from_secs(violation_reset_hours * 3600),
        }
    }

    pub async fn check_rate_limit(&self, user_id: UserId) -> bool {
        let now = Instant::now();
        let mut users = self.users.lock().await;
        let user_state = users.entry(user_id).or_insert_with(|| UserState {
            messages: Vec::new(),
            violation_count: 0,
            cooldown_until: None,
        });

        // クールダウン中かチェック
        if let Some(cooldown_until) = user_state.cooldown_until {
            if now < cooldown_until {
                return false;
            }
            // クールダウンが終了したら、violation_countをリセットするかチェック
            if let Some(last_message) = user_state.messages.last() {
                if now.duration_since(*last_message) >= self.violation_reset_time {
                    user_state.violation_count = 0;
                }
            }
        }

        // 古いメッセージを削除
        user_state.messages.retain(|time| now.duration_since(*time) <= self.time_window);

        // メッセージ数をチェック
        if user_state.messages.len() >= self.max_messages {
            // 違反回数を増やしてクールダウンを設定
            user_state.violation_count += 1;
            
            // クールダウン時間を計算（基本時間 × 乗数^違反回数）
            let cooldown_duration = Duration::from_secs_f32(
                self.base_cooldown.as_secs_f32() * 
                self.cooldown_multiplier.powi(user_state.violation_count as i32)
            );
            
            // 最大クールダウン時間を超えないように調整
            let cooldown_duration = cooldown_duration.min(self.max_cooldown);
            user_state.cooldown_until = Some(now + cooldown_duration);
            
            return false;
        }

        // 新しいメッセージを履歴に追加
        user_state.messages.push(now);
        true
    }

    // 特定ユーザーの現在の状態を取得するメソッド
    pub async fn get_user_state(&self, user_id: UserId) -> Option<(usize, Option<Duration>)> {
        let users = self.users.lock().await;
        users.get(&user_id).map(|state| {
            let remaining_cooldown = state.cooldown_until.map(|until| {
                if Instant::now() < until {
                    until - Instant::now()
                } else {
                    Duration::from_secs(0)
                }
            });
            (state.violation_count, remaining_cooldown)
        })
    }
}
