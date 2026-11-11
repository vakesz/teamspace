use crate::ui::reactions::ReactionSeed;

/// Read-only source material for the local Teams preview.
///
/// The Teams feature owns local user state (drafts, posts, notes, and replies).
/// This module only answers which team and reference posts a selected channel
/// starts with, so every caller uses the same catalog.
#[derive(Clone, Copy)]
pub(crate) struct TeamDefinition {
    pub(crate) name: &'static str,
    pub(crate) channels: &'static [ChannelDefinition],
}

#[derive(Clone, Copy)]
pub(crate) struct ChannelDefinition {
    pub(crate) name: &'static str,
    pub(crate) posts: &'static [SeededChannelPost],
}

#[derive(Clone, Copy)]
pub(crate) struct SeededChannelPost {
    pub(crate) key: &'static str,
    pub(crate) author: &'static str,
    pub(crate) date: &'static str,
    pub(crate) title: &'static str,
    pub(crate) body: &'static str,
    pub(crate) link: Option<&'static str>,
    pub(crate) reactions: &'static [ReactionSeed],
    pub(crate) source_reply_count: usize,
    pub(crate) source_replies: &'static [SourceReply],
}

#[derive(Clone, Copy)]
pub(crate) struct SourceReply {
    /// Stable source-owned identity used by local reaction projections.
    pub(crate) key: &'static str,
    pub(crate) author: &'static str,
    pub(crate) date: &'static str,
    pub(crate) body: &'static str,
    pub(crate) reactions: &'static [ReactionSeed],
}

pub(crate) fn team_catalog() -> &'static [TeamDefinition] {
    &TEAM_CATALOG
}

pub(crate) fn channel_key(team: &TeamDefinition, channel: &ChannelDefinition) -> String {
    format!("{} · {}", team.name, channel.name)
}

pub(crate) fn team_for_channel(channel_key: &str) -> Option<&'static TeamDefinition> {
    TEAM_CATALOG.iter().find(|team| {
        channel_key
            .strip_prefix(team.name)
            .is_some_and(|rest| rest.starts_with(" · "))
    })
}

pub(crate) fn channel_for_key(channel_key: &str) -> Option<&'static ChannelDefinition> {
    TEAM_CATALOG.iter().find_map(|team| {
        team.channels
            .iter()
            .find(|channel| channel_key == channel_key_for(team.name, channel.name))
    })
}

/// Resolves one immutable catalogued post by its stable channel and post
/// identity. Notifications use this instead of duplicating discussion text,
/// reply seeds, or reaction counts for a post they share with Teams.
pub(crate) fn seeded_post_for_channel_key(
    channel_key: &str,
    post_key: &str,
) -> Option<&'static SeededChannelPost> {
    channel_for_key(channel_key)?
        .posts
        .iter()
        .find(|post| post.key == post_key)
}

pub(crate) fn channel_key_for(team: &str, channel: &str) -> String {
    format!("{team} · {channel}")
}

const NO_SOURCE_REPLIES: &[SourceReply] = &[];
const NO_REACTIONS: &[ReactionSeed] = &[];

const PARKING_CHANGES_REPLIES: &[SourceReply] = &[
    SourceReply {
        key: "gate-question",
        author: "Ysolde Varga",
        date: "Yesterday 16:02",
        body: "Would it be possible to put a few covered bike racks near the front door while the work is going on?\n\nMany of us cycle in, and the front railings fill up fast.\n\nCheers!",
        reactions: &[ReactionSeed {
            icon: "❤",
            count: 1,
        }],
    },
    SourceReply {
        key: "gate-response",
        author: "Gideon Pruitt",
        date: "Yesterday 17:31",
        body: "Good idea, I will raise it with the contractor and the landlord this week. No promise yet, though; the aim is to keep the number of bike spaces about the same.",
        reactions: &[ReactionSeed {
            icon: "❤",
            count: 2,
        }],
    },
    SourceReply {
        key: "gate-follow-up",
        author: "Felix Hartmann",
        date: "Today 08:47",
        body: "Will visitor parking stay open during the work? A client is dropping by on the 20th.",
        reactions: NO_REACTIONS,
    },
];

const LOCKER_ACCESS_REPLIES: &[SourceReply] = &[
    SourceReply {
        key: "fob-update",
        author: "Wren Castellanos",
        date: "2025. 05. 06. 13:22",
        body: "I'll look at the door log tomorrow and list which fobs are active. The front desk may also have a spare.",
        reactions: NO_REACTIONS,
    },
    SourceReply {
        key: "looking-into-it",
        author: "Teodor Lindahl",
        date: "2025. 05. 06. 13:51",
        body: "Thanks, I'm already going through the requests.",
        reactions: NO_REACTIONS,
    },
];

const LUNCH_WALKS_GENERAL_POSTS: &[SeededChannelPost] = &[SeededChannelPost {
    key: "lunch-walk",
    author: "Marit Solberg",
    date: "Today 11:25",
    title: "Thursday loop: river path",
    body: "Meet at the front entrance at 12:15 for a 40-minute stroll along the river. Sturdy shoes help, and first-timers can tag along.",
    link: None,
    reactions: &[ReactionSeed {
        icon: "🙌",
        count: 6,
    }],
    source_reply_count: 2,
    source_replies: NO_SOURCE_REPLIES,
}];

const ALL_HANDS_GENERAL_POSTS: &[SeededChannelPost] = &[SeededChannelPost {
    key: "lunch-rota",
    author: "Office Services",
    date: "Yesterday 13:35",
    title: "Lunch rota reminder",
    body: "Names for the shared lunch rota are due by Friday. The sheet is in the kitchen; the first cooking day is Monday at noon.",
    link: None,
    reactions: &[ReactionSeed {
        icon: "👍",
        count: 8,
    }],
    source_reply_count: 3,
    source_replies: NO_SOURCE_REPLIES,
}];

const FACILITIES_POSTS: &[SeededChannelPost] = &[SeededChannelPost {
    key: "parking-changes",
    author: "Gideon Pruitt",
    date: "Yesterday 15:08",
    title: "Car park resurfacing schedule",
    body: "Hello everyone at Juniper Row,\n\nThe rear car park shuts on 18 January 2027 while the surface is relaid, and the side gate by the loading bay gets repainted from 25 January 2027.\n\nCrews expect dry, well-lit bays by the end of February, which should help through the wetter months.\n\nUntil then, please take bikes and any spare kit home or into the workshop store before the first closure date.",
    link: None,
    reactions: &[
        ReactionSeed {
            icon: "😮",
            count: 3,
        },
        ReactionSeed {
            icon: "👍",
            count: 7,
        },
    ],
    source_reply_count: 3,
    source_replies: PARKING_CHANGES_REPLIES,
}];

const WORKSHOP_GUIDES_POSTS: &[SeededChannelPost] = &[SeededChannelPost {
    key: "drill-guide",
    author: "Site Services",
    date: "Monday 10:40",
    title: "Fire drill guide ready for review",
    body: "The assembly point and exit routes have been rewritten. If you have spotted anything confusing on a walkthrough, add a note for newcomers.",
    link: None,
    reactions: &[ReactionSeed {
        icon: "💡",
        count: 4,
    }],
    source_reply_count: 0,
    source_replies: NO_SOURCE_REPLIES,
}];

const WELCOME_POSTS: &[SeededChannelPost] = &[SeededChannelPost {
    key: "welcome-new-starters",
    author: "Office Services",
    date: "Monday 15:20",
    title: "Welcome, new starters",
    body: "Ask about desk setups, say hello, or follow the first-week tour here.",
    link: None,
    reactions: &[ReactionSeed {
        icon: "🎉",
        count: 11,
    }],
    source_reply_count: 4,
    source_replies: NO_SOURCE_REPLIES,
}];

const ODDS_AND_ENDS_POSTS: &[SeededChannelPost] = &[SeededChannelPost {
    key: "handy-trick",
    author: "Juniper Row All Hands",
    date: "Friday 09:55",
    title: "Share a handy trick",
    body: "Small finds and odd questions that fit nowhere else can go here.",
    link: None,
    reactions: &[ReactionSeed {
        icon: "💬",
        count: 2,
    }],
    source_reply_count: 0,
    source_replies: NO_SOURCE_REPLIES,
}];

const GALLERY_POSTS: &[SeededChannelPost] = &[SeededChannelPost {
    key: "open-day-snapshots",
    author: "Studio Notes",
    date: "Thursday 16:48",
    title: "Open day snapshots",
    body: "Great pictures from Saturday's open day! A short caption helps when you add more.",
    link: None,
    reactions: &[ReactionSeed {
        icon: "📷",
        count: 9,
    }],
    source_reply_count: 1,
    source_replies: NO_SOURCE_REPLIES,
}];

const BIKE_COMMUTERS_GENERAL_POSTS: &[SeededChannelPost] = &[
    SeededChannelPost {
        key: "cycle-shelter-quote",
        author: "Wren Castellanos",
        date: "2025. 06. 17. 08:52",
        title: "Cycle Shelter + Ashfield Metalworks",
        body: "Two quotes for the bike shelter are in the link below, each with a layout sketch.\n\nNeither is settled. Treat the numbers as a rough guide for now.\n\nThe site drawing and the parts list show how Ashfield would put the frame together, so anyone handy with tools may spot problems early.",
        link: Some("Cycle Shelter Quote – Site Drawing and Materials"),
        reactions: &[ReactionSeed {
            icon: "👍",
            count: 1,
        }],
        source_reply_count: 0,
        source_replies: NO_SOURCE_REPLIES,
    },
    SeededChannelPost {
        key: "locker-access",
        author: "Teodor Lindahl",
        date: "2025. 05. 06. 13:21",
        title: "Access to the Bike Locker Room",
        body: "Taking over the key-fob requests for the locker room starting today, beginning with the pending one from Noelle.\n\nWren Castellanos: I can check the door log tomorrow and see which fobs are active.",
        link: None,
        reactions: NO_REACTIONS,
        source_reply_count: 12,
        source_replies: LOCKER_ACCESS_REPLIES,
    },
];

const GEAR_SWAP_POSTS: &[SeededChannelPost] = &[SeededChannelPost {
    key: "gear-swap-plan",
    author: "Elias Brandt",
    date: "Today 09:34",
    title: "Gear swap collection plan",
    body: "Boxes can be dropped at the side door from Saturday. Tag anything needing a repair so it is kept off the public list.",
    link: None,
    reactions: &[ReactionSeed {
        icon: "✅",
        count: 5,
    }],
    source_reply_count: 2,
    source_replies: NO_SOURCE_REPLIES,
}];

const SWAP_SHELF_POSTS: &[SeededChannelPost] = &[SeededChannelPost {
    key: "december-giveaways",
    author: "Swap Shelf",
    date: "Today 07:55",
    title: "December giveaways",
    body: "Add a photo, the size, and where it can be collected, and it will go fast.",
    link: None,
    reactions: &[ReactionSeed {
        icon: "🛍",
        count: 3,
    }],
    source_reply_count: 0,
    source_replies: NO_SOURCE_REPLIES,
}];

const BOOK_CORNER_POSTS: &[SeededChannelPost] = &[SeededChannelPost {
    key: "winter-reading",
    author: "Book Corner",
    date: "Wednesday 12:10",
    title: "Winter reading list",
    body: "What are you reading this winter? Post reviews and suggest the next title for the group.",
    link: None,
    reactions: &[ReactionSeed {
        icon: "🙌",
        count: 4,
    }],
    source_reply_count: 1,
    source_replies: NO_SOURCE_REPLIES,
}];

const LUNCH_WALKS_CHANNELS: &[ChannelDefinition] = &[ChannelDefinition {
    name: "General",
    posts: LUNCH_WALKS_GENERAL_POSTS,
}];

const ALL_HANDS_CHANNELS: &[ChannelDefinition] = &[
    ChannelDefinition {
        name: "General",
        posts: ALL_HANDS_GENERAL_POSTS,
    },
    ChannelDefinition {
        name: "Facilities",
        posts: FACILITIES_POSTS,
    },
    ChannelDefinition {
        name: "Workshop Guides",
        posts: WORKSHOP_GUIDES_POSTS,
    },
    ChannelDefinition {
        name: "Welcome",
        posts: WELCOME_POSTS,
    },
    ChannelDefinition {
        name: "Odds and Ends",
        posts: ODDS_AND_ENDS_POSTS,
    },
    ChannelDefinition {
        name: "Gallery",
        posts: GALLERY_POSTS,
    },
];

const BIKE_COMMUTERS_CHANNELS: &[ChannelDefinition] = &[
    ChannelDefinition {
        name: "General",
        posts: BIKE_COMMUTERS_GENERAL_POSTS,
    },
    ChannelDefinition {
        name: "Winter Gear Swap",
        posts: GEAR_SWAP_POSTS,
    },
];

const SWAP_SHELF_CHANNELS: &[ChannelDefinition] = &[ChannelDefinition {
    name: "General",
    posts: SWAP_SHELF_POSTS,
}];

const BOOK_CORNER_CHANNELS: &[ChannelDefinition] = &[ChannelDefinition {
    name: "General",
    posts: BOOK_CORNER_POSTS,
}];

const TEAM_CATALOG: [TeamDefinition; 5] = [
    TeamDefinition {
        name: "Lunchtime Walks",
        channels: LUNCH_WALKS_CHANNELS,
    },
    TeamDefinition {
        name: "Juniper Row All Hands",
        channels: ALL_HANDS_CHANNELS,
    },
    TeamDefinition {
        name: "Bike Commuters",
        channels: BIKE_COMMUTERS_CHANNELS,
    },
    TeamDefinition {
        name: "Swap Shelf",
        channels: SWAP_SHELF_CHANNELS,
    },
    TeamDefinition {
        name: "Book Corner",
        channels: BOOK_CORNER_CHANNELS,
    },
];

#[cfg(test)]
mod tests {
    use super::{channel_for_key, channel_key, team_catalog, team_for_channel};

    #[test]
    fn every_catalogued_channel_has_reference_posts_and_resolves_from_its_key() {
        for team in team_catalog() {
            for channel in team.channels {
                let key = channel_key(team, channel);
                assert!(!channel.posts.is_empty(), "{key} needs a source post");
                assert!(
                    channel
                        .posts
                        .iter()
                        .all(|post| { post.source_replies.len() <= post.source_reply_count })
                );
                assert_eq!(
                    team_for_channel(&key).map(|team| team.name),
                    Some(team.name)
                );
                assert_eq!(
                    channel_for_key(&key).map(|channel| channel.name),
                    Some(channel.name)
                );
            }
        }
    }

    #[test]
    fn unknown_keys_do_not_resolve_to_a_catalogued_channel() {
        assert!(team_for_channel("Unknown team · General").is_none());
        assert!(channel_for_key("Bike Commuters · Unknown").is_none());
    }
}
