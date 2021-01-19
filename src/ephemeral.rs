//! # Ephemeral messages
//!
//! Ephemeral messages are messages that have an Ephemeral-Timer
//! header attached to them, which specifies time in seconds after
//! which the message should be deleted both from the device and from
//! the server. The timer is started when the message is marked as
//! seen, which usually happens when its contents is displayed on
//! device screen.
//!
//! Each chat, including 1:1, group chats and "saved messages" chat,
//! has its own ephemeral timer setting, which is applied to all
//! messages sent to the chat. The setting is synchronized to all the
//! devices participating in the chat by applying the timer value from
//! all received messages, including BCC-self ones, to the chat. This
//! way the setting is eventually synchronized among all participants.
//!
//! When user changes ephemeral timer setting for the chat, a system
//! message is automatically sent to update the setting for all
//! participants. This allows changing the setting for a chat like any
//! group chat setting, e.g. name and avatar, without the need to
//! write an actual message.
//!
//! ## Device settings
//!
//! In addition to per-chat ephemeral message setting, each device has
//! two global user-configured settings that complement per-chat
//! settings: `delete_device_after` and `delete_server_after`. These
//! settings are not synchronized among devices and apply to all
//! messages known to the device, including messages sent or received
//! before configuring the setting.
//!
//! `delete_device_after` configures the maximum time device is
//! storing the messages locally. `delete_server_after` configures the
//! time after which device will delete the messages it knows about
//! from the server.
//!
//! ## How messages are deleted
//!
//! When the message is deleted locally, its contents is removed and
//! it is moved to the trash chat. This database entry is then used to
//! track the Message-ID and corresponding IMAP folder and UID until
//! the message is deleted from the server. Vice versa, when device
//! deletes the message from the server, it removes IMAP folder and
//! UID information, but keeps the message contents. When database
//! entry is both moved to trash chat and does not contain UID
//! information, it is deleted from the database, leaving no trace of
//! the message.
//!
//! ## When messages are deleted
//!
//! Local deletion happens when the chatlist or chat is loaded. A
//! `MsgsChanged` event is emitted when a message deletion is due, to
//! make UI reload displayed messages and cause actual deletion.
//!
//! Server deletion happens by generating IMAP deletion jobs based on
//! the database entries which are expired either according to their
//! ephemeral message timers or global `delete_server_after` setting.

use crate::chat::{lookup_by_contact_id, send_msg, ChatId};
use crate::constants::{
    Viewtype, DC_CHAT_ID_LAST_SPECIAL, DC_CHAT_ID_TRASH, DC_CONTACT_ID_DEVICE, DC_CONTACT_ID_SELF,
};
use crate::context::Context;
use crate::dc_tools::time;
use crate::error::{ensure, Error};
use crate::events::EventType;
use crate::message::{Message, MessageState, MsgId};
use crate::mimeparser::SystemMessage;
use crate::sql;
use crate::stock::StockMessage;
use async_std::task;
use serde::{Deserialize, Serialize};
use std::convert::{TryFrom, TryInto};
use std::num::ParseIntError;
use std::str::FromStr;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[derive(Debug, PartialEq, Eq, Copy, Clone, Serialize, Deserialize)]
pub enum Timer {
    Disabled,
    Enabled { duration: u32 },
}

impl Timer {
    pub fn to_u32(self) -> u32 {
        match self {
            Self::Disabled => 0,
            Self::Enabled { duration } => duration,
        }
    }

    pub fn from_u32(duration: u32) -> Self {
        if duration == 0 {
            Self::Disabled
        } else {
            Self::Enabled { duration }
        }
    }
}

impl Default for Timer {
    fn default() -> Self {
        Self::Disabled
    }
}

impl ToString for Timer {
    fn to_string(&self) -> String {
        self.to_u32().to_string()
    }
}

impl FromStr for Timer {
    type Err = ParseIntError;

    fn from_str(input: &str) -> Result<Timer, ParseIntError> {
        input.parse::<u32>().map(Self::from_u32)
    }
}

impl rusqlite::types::ToSql for Timer {
    fn to_sql(&self) -> rusqlite::Result<rusqlite::types::ToSqlOutput> {
        let val = rusqlite::types::Value::Integer(match self {
            Self::Disabled => 0,
            Self::Enabled { duration } => i64::from(*duration),
        });
        let out = rusqlite::types::ToSqlOutput::Owned(val);
        Ok(out)
    }
}

impl rusqlite::types::FromSql for Timer {
    fn column_result(value: rusqlite::types::ValueRef) -> rusqlite::types::FromSqlResult<Self> {
        i64::column_result(value).and_then(|value| {
            if value == 0 {
                Ok(Self::Disabled)
            } else if let Ok(duration) = u32::try_from(value) {
                Ok(Self::Enabled { duration })
            } else {
                Err(rusqlite::types::FromSqlError::OutOfRange(value))
            }
        })
    }
}

impl ChatId {
    /// Get ephemeral message timer value in seconds.
    pub async fn get_ephemeral_timer(self, context: &Context) -> Result<Timer, Error> {
        let timer = context
            .sql
            .query_get_value_result(
                "SELECT ephemeral_timer FROM chats WHERE id=?;",
                paramsv![self],
            )
            .await?;
        Ok(timer.unwrap_or_default())
    }

    /// Set ephemeral timer value without sending a message.
    ///
    /// Used when a message arrives indicating that someone else has
    /// changed the timer value for a chat.
    pub(crate) async fn inner_set_ephemeral_timer(
        self,
        context: &Context,
        timer: Timer,
    ) -> Result<(), Error> {
        ensure!(!self.is_special(), "Invalid chat ID");

        context
            .sql
            .execute(
                "UPDATE chats
             SET ephemeral_timer=?
             WHERE id=?;",
                paramsv![timer, self],
            )
            .await?;

        context.emit_event(EventType::ChatEphemeralTimerModified {
            chat_id: self,
            timer,
        });
        Ok(())
    }

    /// Set ephemeral message timer value in seconds.
    ///
    /// If timer value is 0, disable ephemeral message timer.
    pub async fn set_ephemeral_timer(self, context: &Context, timer: Timer) -> Result<(), Error> {
        if timer == self.get_ephemeral_timer(context).await? {
            return Ok(());
        }
        self.inner_set_ephemeral_timer(context, timer).await?;
        let mut msg = Message::new(Viewtype::Text);
        msg.text = Some(stock_ephemeral_timer_changed(context, timer, DC_CONTACT_ID_SELF).await);
        msg.param.set_cmd(SystemMessage::EphemeralTimerChanged);
        if let Err(err) = send_msg(context, self, &mut msg).await {
            error!(
                context,
                "Failed to send a message about ephemeral message timer change: {:?}", err
            );
        }
        Ok(())
    }
}

/// Returns a stock message saying that ephemeral timer is changed to `timer` by `from_id`.
pub(crate) async fn stock_ephemeral_timer_changed(
    context: &Context,
    timer: Timer,
    from_id: u32,
) -> String {
    let stock_message = match timer {
        Timer::Disabled => StockMessage::MsgEphemeralTimerDisabled,
        Timer::Enabled { duration } => match duration {
            60 => StockMessage::MsgEphemeralTimerMinute,
            3600 => StockMessage::MsgEphemeralTimerHour,
            86400 => StockMessage::MsgEphemeralTimerDay,
            604_800 => StockMessage::MsgEphemeralTimerWeek,
            2_419_200 => StockMessage::MsgEphemeralTimerFourWeeks,
            _ => StockMessage::MsgEphemeralTimerEnabled,
        },
    };

    context
        .stock_system_msg(stock_message, timer.to_string(), "", from_id)
        .await
}

impl MsgId {
    /// Returns ephemeral message timer value for the message.
    pub(crate) async fn ephemeral_timer(self, context: &Context) -> crate::sql::Result<Timer> {
        let res = match context
            .sql
            .query_get_value_result(
                "SELECT ephemeral_timer FROM msgs WHERE id=?",
                paramsv![self],
            )
            .await?
        {
            None | Some(0) => Timer::Disabled,
            Some(duration) => Timer::Enabled { duration },
        };
        Ok(res)
    }

    /// Starts ephemeral message timer for the message if it is not started yet.
    pub(crate) async fn start_ephemeral_timer(self, context: &Context) -> crate::sql::Result<()> {
        if let Timer::Enabled { duration } = self.ephemeral_timer(context).await? {
            let ephemeral_timestamp = time() + i64::from(duration);

            context
                .sql
                .execute(
                    "UPDATE msgs SET ephemeral_timestamp = ? \
                WHERE (ephemeral_timestamp == 0 OR ephemeral_timestamp > ?) \
                AND id = ?",
                    paramsv![ephemeral_timestamp, ephemeral_timestamp, self],
                )
                .await?;
            schedule_ephemeral_task(context).await;
        }
        Ok(())
    }
}

/// Deletes messages which are expired according to
/// `delete_device_after` setting or `ephemeral_timestamp` column.
///
/// Returns true if any message is deleted, so caller can emit
/// MsgsChanged event. If nothing has been deleted, returns
/// false. This function does not emit the MsgsChanged event itself,
/// because it is also called when chatlist is reloaded, and emitting
/// MsgsChanged there will cause infinite reload loop.
pub(crate) async fn delete_expired_messages(context: &Context) -> Result<bool, Error> {
    let mut updated = context
        .sql
        .execute(
            "UPDATE msgs \
             SET txt = 'DELETED', chat_id = ? \
             WHERE \
             ephemeral_timestamp != 0 \
             AND ephemeral_timestamp < ? \
             AND chat_id != ?",
            paramsv![DC_CHAT_ID_TRASH, time(), DC_CHAT_ID_TRASH],
        )
        .await?
        > 0;

    if let Some(delete_device_after) = context.get_config_delete_device_after().await {
        let self_chat_id = lookup_by_contact_id(context, DC_CONTACT_ID_SELF)
            .await
            .unwrap_or_default()
            .0;
        let device_chat_id = lookup_by_contact_id(context, DC_CONTACT_ID_DEVICE)
            .await
            .unwrap_or_default()
            .0;

        let threshold_timestamp = time() - delete_device_after;

        // Delete expired messages
        //
        // Only update the rows that have to be updated, to avoid emitting
        // unnecessary "chat modified" events.
        let rows_modified = context
            .sql
            .execute(
                "UPDATE msgs \
             SET txt = 'DELETED', chat_id = ? \
             WHERE timestamp < ? \
             AND chat_id > ? \
             AND chat_id != ? \
             AND chat_id != ?",
                paramsv![
                    DC_CHAT_ID_TRASH,
                    threshold_timestamp,
                    DC_CHAT_ID_LAST_SPECIAL,
                    self_chat_id,
                    device_chat_id
                ],
            )
            .await?;

        updated |= rows_modified > 0;
    }

    schedule_ephemeral_task(context).await;
    Ok(updated)
}

/// Schedule a task to emit MsgsChanged event when the next local
/// deletion happens. Existing task is cancelled to make sure at most
/// one such task is scheduled at a time.
///
/// UI is expected to reload the chatlist or the chat in response to
/// MsgsChanged event, this will trigger actual deletion.
///
/// This takes into account only per-chat timeouts, because global device
/// timeouts are at least one hour long and deletion is triggered often enough
/// by user actions.
pub async fn schedule_ephemeral_task(context: &Context) {
    let ephemeral_timestamp: Option<i64> = match context
        .sql
        .query_get_value_result(
            "SELECT ephemeral_timestamp \
         FROM msgs \
         WHERE ephemeral_timestamp != 0 \
           AND chat_id != ? \
         ORDER BY ephemeral_timestamp ASC \
         LIMIT 1",
            paramsv![DC_CHAT_ID_TRASH], // Trash contains already deleted messages, skip them
        )
        .await
    {
        Err(err) => {
            warn!(context, "Can't calculate next ephemeral timeout: {}", err);
            return;
        }
        Ok(ephemeral_timestamp) => ephemeral_timestamp,
    };

    // Cancel existing task, if any
    if let Some(ephemeral_task) = context.ephemeral_task.write().await.take() {
        ephemeral_task.cancel().await;
    }

    if let Some(ephemeral_timestamp) = ephemeral_timestamp {
        let now = SystemTime::now();
        let until = UNIX_EPOCH
            + Duration::from_secs(ephemeral_timestamp.try_into().unwrap_or(u64::MAX))
            + Duration::from_secs(1);

        if let Ok(duration) = until.duration_since(now) {
            // Schedule a task, ephemeral_timestamp is in the future
            let context1 = context.clone();
            let ephemeral_task = task::spawn(async move {
                async_std::task::sleep(duration).await;
                emit_event!(
                    context1,
                    EventType::MsgsChanged {
                        chat_id: ChatId::new(0),
                        msg_id: MsgId::new(0)
                    }
                );
            });
            *context.ephemeral_task.write().await = Some(ephemeral_task);
        } else {
            // Emit event immediately
            emit_event!(
                context,
                EventType::MsgsChanged {
                    chat_id: ChatId::new(0),
                    msg_id: MsgId::new(0)
                }
            );
        }
    }
}

/// Returns ID of any expired message that should be deleted from the server.
///
/// It looks up the trash chat too, to find messages that are already
/// deleted locally, but not deleted on the server.
pub(crate) async fn load_imap_deletion_msgid(context: &Context) -> sql::Result<Option<MsgId>> {
    let now = time();

    let threshold_timestamp = match context.get_config_delete_server_after().await {
        None => 0,
        Some(delete_server_after) => now - delete_server_after,
    };

    context
        .sql
        .query_row_optional(
            "SELECT id FROM msgs \
         WHERE ( \
         timestamp < ? \
         OR (ephemeral_timestamp != 0 AND ephemeral_timestamp < ?) \
         ) \
         AND server_uid != 0 \
         LIMIT 1",
            paramsv![threshold_timestamp, now],
            |row| row.get::<_, MsgId>(0),
        )
        .await
}

/// Start ephemeral timers for seen messages if they are not started
/// yet.
///
/// It is possible that timers are not started due to a missing or
/// failed `MsgId.start_ephemeral_timer()` call, either in the current
/// or previous version of Delta Chat.
///
/// This function is supposed to be called in the background,
/// e.g. from housekeeping task.
pub(crate) async fn start_ephemeral_timers(context: &Context) -> sql::Result<()> {
    context
        .sql
        .execute(
            "UPDATE msgs \
    SET ephemeral_timestamp = ? + ephemeral_timer \
    WHERE ephemeral_timer > 0 \
    AND ephemeral_timestamp = 0 \
    AND state NOT IN (?, ?, ?)",
            paramsv![
                time(),
                MessageState::InFresh,
                MessageState::InNoticed,
                MessageState::OutDraft
            ],
        )
        .await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::*;

    #[async_std::test]
    async fn test_stock_ephemeral_messages() {
        let context = TestContext::new().await.ctx;

        assert_eq!(
            stock_ephemeral_timer_changed(&context, Timer::Disabled, DC_CONTACT_ID_SELF).await,
            "Message deletion timer is disabled by me."
        );

        assert_eq!(
            stock_ephemeral_timer_changed(&context, Timer::Disabled, 0).await,
            "Message deletion timer is disabled."
        );
        assert_eq!(
            stock_ephemeral_timer_changed(&context, Timer::Enabled { duration: 1 }, 0).await,
            "Message deletion timer is set to 1 s."
        );
        assert_eq!(
            stock_ephemeral_timer_changed(&context, Timer::Enabled { duration: 30 }, 0).await,
            "Message deletion timer is set to 30 s."
        );
        assert_eq!(
            stock_ephemeral_timer_changed(&context, Timer::Enabled { duration: 60 }, 0).await,
            "Message deletion timer is set to 1 minute."
        );
        assert_eq!(
            stock_ephemeral_timer_changed(&context, Timer::Enabled { duration: 60 * 60 }, 0).await,
            "Message deletion timer is set to 1 hour."
        );
        assert_eq!(
            stock_ephemeral_timer_changed(
                &context,
                Timer::Enabled {
                    duration: 24 * 60 * 60
                },
                0
            )
            .await,
            "Message deletion timer is set to 1 day."
        );
        assert_eq!(
            stock_ephemeral_timer_changed(
                &context,
                Timer::Enabled {
                    duration: 7 * 24 * 60 * 60
                },
                0
            )
            .await,
            "Message deletion timer is set to 1 week."
        );
        assert_eq!(
            stock_ephemeral_timer_changed(
                &context,
                Timer::Enabled {
                    duration: 4 * 7 * 24 * 60 * 60
                },
                0
            )
            .await,
            "Message deletion timer is set to 4 weeks."
        );
    }
}
