use rxcord_api::event::Event;
use rxcord_api::id::Id;
use rxcord_api::model::{Channel, Guild, Message, User};
use std::collections::{HashMap, HashSet, VecDeque};

static EMPTY: VecDeque<Message> = VecDeque::new();

pub const MESSAGE_CAP: usize = 200;

#[derive(Clone, PartialEq, Debug)]
pub enum Change {
    Me,
    GuildList,
    Channels { guild: Option<Id> },
    DmList,
    Messages(Id),
    Unread(Id),
}

#[derive(Clone, PartialEq, Debug)]
pub struct Pending {
    pub nonce: String,
    pub channel: Id,
    pub content: String,
    pub reply_to: Option<Id>,
    pub failed: bool,
}

#[derive(Default)]
struct Read {
    last_read: Option<Id>,
    mentions: u32,
}

#[derive(Default)]
pub struct Store {
    me: Option<User>,
    guilds: Vec<Guild>,
    channels: HashMap<Id, Channel>,
    users: HashMap<Id, User>,
    read: HashMap<Id, Read>,
    msgs: HashMap<Id, VecDeque<Message>>,
    pending: Vec<Pending>,
}

fn is_dm(c: &Channel) -> bool {
    c.kind == 1 || c.kind == 3
}

impl Store {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn me(&self) -> Option<&User> {
        self.me.as_ref()
    }
    pub fn guilds(&self) -> &[Guild] {
        &self.guilds
    }
    pub fn channel(&self, id: Id) -> Option<&Channel> {
        self.channels.get(&id)
    }
    pub fn user(&self, id: Id) -> Option<&User> {
        self.users.get(&id)
    }
    pub fn messages(&self, channel: Id) -> &VecDeque<Message> {
        self.msgs.get(&channel).unwrap_or(&EMPTY)
    }
    /// Every unsent message, in send order.
    pub fn all_pending(&self) -> &[Pending] {
        &self.pending
    }
    pub fn pending(&self, channel: Id) -> Vec<&Pending> {
        self.pending.iter().filter(|p| p.channel == channel).collect()
    }
    pub fn mention_count(&self, channel: Id) -> u32 {
        self.read.get(&channel).map_or(0, |r| r.mentions)
    }
    pub fn is_unread(&self, channel: Id) -> bool {
        let last = self.channels.get(&channel).and_then(|c| c.last_message_id);
        let read = self.read.get(&channel).and_then(|r| r.last_read);
        last.is_some() && last > read
    }

    pub fn channels_of(&self, guild: Id) -> Vec<&Channel> {
        let mut v: Vec<_> = self.channels.values().filter(|c| c.guild_id == Some(guild)).collect();
        v.sort_by_key(|c| (c.position, c.id));
        v
    }
    pub fn dms(&self) -> Vec<&Channel> {
        let mut v: Vec<_> = self.channels.values().filter(|c| is_dm(c)).collect();
        v.sort_by_key(|c| std::cmp::Reverse((c.last_message_id, c.id)));
        v
    }

    fn put_channel(&mut self, mut c: Channel, guild: Option<Id>) {
        if guild.is_some() {
            c.guild_id = guild;
        }
        for u in std::mem::take(&mut c.recipients) {
            if !c.recipient_ids.contains(&u.id) {
                c.recipient_ids.push(u.id);
            }
            self.users.entry(u.id).or_insert(u);
        }
        self.channels.insert(c.id, c);
    }

    fn mark_read(&mut self, channel: Id, upto: Id) {
        let r = self.read.entry(channel).or_default();
        r.last_read = r.last_read.max(Some(upto));
        r.mentions = 0;
    }

    /// READY replaces all state except `pending` (unsent messages survive a
    /// re-identify). Messages are dropped as they may have a gap; the app
    /// re-opens the selected channel afterwards (`open_channel` -> true -> refetch).
    pub fn apply(&mut self, ev: &Event) -> Vec<Change> {
        match ev {
            Event::Ready(r) => {
                let pending = std::mem::take(&mut self.pending);
                *self = Store::default();
                self.pending = pending;
                self.me = Some(r.user.clone());
                for u in &r.users {
                    self.users.insert(u.id, u.clone());
                }
                self.users.insert(r.user.id, r.user.clone());
                for g in &r.guilds {
                    for c in &g.channels {
                        self.put_channel(c.clone(), Some(g.id));
                    }
                    let mut g = g.clone();
                    g.channels.clear();
                    self.guilds.push(g);
                }
                for c in &r.private_channels {
                    self.put_channel(c.clone(), None);
                }
                for e in &r.read_state {
                    self.read.insert(e.id, Read { last_read: e.last_message_id, mentions: e.mention_count });
                }
                vec![Change::Me, Change::GuildList, Change::Channels { guild: None }, Change::DmList]
            }
            Event::GuildCreate(g) => {
                for c in &g.channels {
                    self.put_channel(c.clone(), Some(g.id));
                }
                let mut g = g.clone();
                g.channels.clear();
                match self.guilds.iter_mut().find(|x| x.id == g.id) {
                    Some(x) => *x = g,
                    None => self.guilds.push(g),
                }
                vec![Change::GuildList, Change::Channels { guild: None }]
            }
            // An outage, not a leave: keep everything; GUILD_CREATE follows when it is back.
            Event::GuildDelete { unavailable: true, .. } => vec![],
            Event::GuildDelete { id, .. } => {
                self.guilds.retain(|g| g.id != *id);
                let gone: Vec<Id> = self.channels.values().filter(|c| c.guild_id == Some(*id)).map(|c| c.id).collect();
                for c in gone {
                    self.drop_channel(c);
                }
                vec![Change::GuildList, Change::Channels { guild: Some(*id) }]
            }
            Event::ChannelCreate(c) | Event::ChannelUpdate(c) => {
                let mut c = c.clone();
                // Updates may omit the last message id; keep ours.
                if c.last_message_id.is_none() {
                    c.last_message_id = self.channels.get(&c.id).and_then(|o| o.last_message_id);
                }
                let ch = self.chg(&c);
                self.put_channel(c, None);
                vec![ch]
            }
            Event::ChannelDelete(c) => {
                self.drop_channel(c.id);
                vec![self.chg(c)]
            }
            Event::MessageCreate(m) => self.on_create(m),
            Event::MessageUpdate(u) => {
                let Some(m) = self.msgs.get_mut(&u.channel_id).and_then(|q| q.iter_mut().find(|m| m.id == u.id))
                else {
                    return vec![];
                };
                if let Some(c) = &u.content {
                    m.content = c.clone();
                }
                if u.edited_timestamp.is_some() {
                    m.edited_timestamp = u.edited_timestamp.clone();
                }
                if let Some(a) = &u.attachments {
                    m.attachments = a.clone();
                }
                vec![Change::Messages(u.channel_id)]
            }
            Event::MessageDelete { id, channel_id } => {
                let Some(q) = self.msgs.get_mut(channel_id) else { return vec![] };
                let n = q.len();
                q.retain(|m| m.id != *id);
                if q.len() == n { vec![] } else { vec![Change::Messages(*channel_id)] }
            }
            Event::MessageAck { channel_id, message_id } => {
                self.mark_read(*channel_id, *message_id);
                vec![Change::Unread(*channel_id)]
            }
            Event::Resumed | Event::Unknown(_) => vec![],
        }
    }

    /// Pending messages of `id` are kept: the app turns them into drafts when the session ends.
    fn drop_channel(&mut self, id: Id) {
        self.channels.remove(&id);
        self.read.remove(&id);
        self.msgs.remove(&id);
    }

    fn chg(&self, c: &Channel) -> Change {
        if is_dm(c) { Change::DmList } else { Change::Channels { guild: c.guild_id } }
    }

    fn on_create(&mut self, m: &Message) -> Vec<Change> {
        let Some(ch) = self.channels.get_mut(&m.channel_id) else {
            tracing::debug!(channel = m.channel_id.0, "message for unknown channel");
            return vec![];
        };
        // Not newer than what we've seen: a redelivery (e.g. after resume).
        let fresh = ch.last_message_id < Some(m.id);
        ch.last_message_id = ch.last_message_id.max(Some(m.id));
        let dm = is_dm(ch);
        let mut out = vec![];
        if self.msgs.contains_key(&m.channel_id) {
            let q = self.msgs.entry(m.channel_id).or_default();
            if q.iter().any(|x| x.id == m.id) {
                return vec![];
            }
            {
                q.push_back(m.clone());
                while q.len() > MESSAGE_CAP {
                    q.pop_front();
                }
            }
            if let Some(n) = &m.nonce {
                self.pending.retain(|p| &p.nonce != n);
            }
            out.push(Change::Messages(m.channel_id));
        }
        if !fresh {
            return out;
        }
        let me = self.me.as_ref().map(|u| u.id);
        if Some(m.author.id) == me {
            self.mark_read(m.channel_id, m.id);
        } else {
            let mentioned = dm || m.mention_everyone || m.mentions.iter().any(|u| Some(u.id) == me);
            if mentioned {
                self.read.entry(m.channel_id).or_default().mentions += 1;
            }
        }
        out.push(Change::Unread(m.channel_id));
        out
    }

    /// True when history must be fetched (nothing stored yet).
    pub fn open_channel(&mut self, id: Id) -> bool {
        self.msgs.entry(id).or_default().is_empty()
    }

    pub fn close_channel(&mut self, id: Id) {
        if let Some(q) = self.msgs.get_mut(&id) {
            let extra = q.len().saturating_sub(MESSAGE_CAP);
            q.drain(..extra);
        }
    }

    /// Drops the loaded messages of `id` (its tab closed); new messages stop being kept until it's reopened.
    pub fn forget_channel(&mut self, id: Id) {
        self.msgs.remove(&id);
    }

    /// `msgs` is newest-first as returned by the API.
    pub fn prepend_history(&mut self, channel: Id, msgs: Vec<Message>) -> Vec<Change> {
        let q = self.msgs.entry(channel).or_default();
        let mut have: HashSet<Id> = q.iter().map(|m| m.id).collect();
        let mut added = false;
        for m in msgs.into_iter().filter(|m| have.insert(m.id)) {
            q.push_front(m);
            added = true;
        }
        if added { vec![Change::Messages(channel)] } else { vec![] }
    }

    pub fn add_pending(&mut self, p: Pending) -> Change {
        let c = Change::Messages(p.channel);
        self.pending.push(p);
        c
    }
    pub fn fail_pending(&mut self, nonce: &str) -> Option<Change> {
        let p = self.pending.iter_mut().find(|p| p.nonce == nonce)?;
        p.failed = true;
        Some(Change::Messages(p.channel))
    }
    /// Clears the failed flag for a resend with the same nonce and returns the entry to resend;
    /// `None` unless it had failed (a send still in flight is not sent twice).
    pub fn retry_pending(&mut self, nonce: &str) -> Option<Pending> {
        let p = self.pending.iter_mut().find(|p| p.nonce == nonce && p.failed)?;
        p.failed = false;
        Some(p.clone())
    }
    pub fn take_pending(&mut self, nonce: &str) -> Option<Pending> {
        let i = self.pending.iter().position(|p| p.nonce == nonce)?;
        Some(self.pending.remove(i))
    }
}
