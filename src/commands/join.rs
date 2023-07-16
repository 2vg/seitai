use anyhow::Result;
use serenity::{
    builder::{CreateCommand, CreateEmbed, CreateInteractionResponseMessage},
    client::Context,
    model::{application::CommandInteraction, Colour},
};

use crate::utils::{get_guild, get_manager, respond};

pub(crate) async fn run(context: &Context, interaction: &CommandInteraction) -> Result<()> {
    let guild = match get_guild(context, interaction) {
        Some(guild) => guild,
        None => {
            let message = CreateInteractionResponseMessage::new().embed(
                CreateEmbed::new()
                    .description("このコマンドは使えません。")
                    .colour(Colour::RED),
            );
            respond(context, interaction, message).await?;
            return Ok(());
        },
    };
    let channel_id = guild
        .voice_states
        .get(&interaction.user.id)
        .and_then(|voice_state| voice_state.channel_id);

    let connect_to = match channel_id {
        Some(channel) => channel,
        None => {
            let message = CreateInteractionResponseMessage::new().embed(
                CreateEmbed::new()
                    .description("接続先のボイスチャンネルが見つかりません。")
                    .colour(Colour::RED),
            );
            respond(context, interaction, message).await?;
            return Ok(());
        },
    };

    let manager = get_manager(context).await?;
    let call = manager.get_or_insert(guild.id);

    let mut handler = call.lock().await;
    handler.deafen(true).await.unwrap();

    let message = CreateInteractionResponseMessage::new().embed(
        CreateEmbed::new()
            .description(
                "現在不具合により、`/join`コマンドを実行した際に`アプリケーションが応答しませんでした`と表示されます。\n".to_string()
                + "また、接続後メッセージを読み上げるようになるまで数秒のラグがあります。"
            )
            .colour(Colour::ORANGE),
    );
    respond(context, interaction, message).await?;

    match handler.join(connect_to).await {
        Ok(join) => {
            match join.await {
                Ok(_) => {},
                Err(why) => {
                    println!("err1: {why}");
                },
            };
        },
        Err(why) => {
            println!("err2: {why}");
        },
    };

    let message = CreateInteractionResponseMessage::new().embed(
        CreateEmbed::new()
            .description("ボイスチャンネルに接続しました。")
            .colour(Colour::FOOYOO),
    );
    respond(context, interaction, message).await?;
    Ok(())
}

pub fn register() -> CreateCommand {
    CreateCommand::new("join").description("ボイスチャンネルに接続します。")
}