//! Direct-address classification (D5): which inbound messages may ever wake a
//! session. A message is a direct address when it arrived in a one-to-one room
//! (a DM) or in the roster room with `m.mentions.user_ids` naming this seat's
//! own MXID. Everything else in the roster room is chatter, and a message in
//! any other room is not ours to read.

use aqua_seat::proto::Via;

/// What the handler knows about one inbound event.
#[derive(Debug, Clone)]
pub struct Facts<'a> {
    pub own: &'a str,
    pub sender: &'a str,
    pub is_roster_room: bool,
    /// The room has exactly two active members, this seat and the sender.
    pub is_one_to_one: bool,
    /// `m.mentions.user_ids` of the event.
    pub mentioned_user_ids: &'a [String],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ignore {
    /// Our own message echoing back.
    Own,
    /// Neither the roster room nor a one-to-one room.
    OtherRoom,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Ignore(Ignore),
    Accept(Via),
}

pub fn classify(f: &Facts<'_>) -> Verdict {
    if f.sender.eq_ignore_ascii_case(f.own) {
        return Verdict::Ignore(Ignore::Own);
    }
    if f.is_roster_room {
        let named = f
            .mentioned_user_ids
            .iter()
            .any(|m| m.eq_ignore_ascii_case(f.own));
        return Verdict::Accept(if named { Via::Mention } else { Via::Room });
    }
    if f.is_one_to_one {
        return Verdict::Accept(Via::Dm);
    }
    Verdict::Ignore(Ignore::OtherRoom)
}

#[cfg(test)]
mod tests {
    use super::*;

    const OWN: &str = "@me:dev.matrix.inblock.io";
    const PEER: &str = "@peer:dev.matrix.inblock.io";

    fn facts<'a>(roster: bool, one: bool, m: &'a [String]) -> Facts<'a> {
        Facts {
            own: OWN,
            sender: PEER,
            is_roster_room: roster,
            is_one_to_one: one,
            mentioned_user_ids: m,
        }
    }

    #[test]
    fn a_dm_is_a_direct_address() {
        assert_eq!(classify(&facts(false, true, &[])), Verdict::Accept(Via::Dm));
    }

    #[test]
    fn a_room_mention_of_me_is_a_direct_address_case_insensitively() {
        let m = vec!["@ME:dev.matrix.inblock.io".to_string()];
        assert_eq!(classify(&facts(true, false, &m)), Verdict::Accept(Via::Mention));
    }

    #[test]
    fn plain_room_chatter_is_never_direct() {
        assert_eq!(classify(&facts(true, false, &[])), Verdict::Accept(Via::Room));
        assert!(!Via::Room.is_direct());
    }

    #[test]
    fn a_mention_of_someone_else_is_chatter_for_me() {
        let m = vec!["@other:dev.matrix.inblock.io".to_string()];
        assert_eq!(classify(&facts(true, false, &m)), Verdict::Accept(Via::Room));
    }

    #[test]
    fn a_two_member_roster_room_is_still_the_roster_not_a_dm() {
        // The first two seats make a roster room with exactly two members.
        assert_eq!(classify(&facts(true, true, &[])), Verdict::Accept(Via::Room));
    }

    #[test]
    fn a_mention_in_a_dm_is_just_a_dm() {
        let m = vec![OWN.to_string()];
        assert_eq!(classify(&facts(false, true, &m)), Verdict::Accept(Via::Dm));
    }

    #[test]
    fn other_group_rooms_and_own_echoes_are_ignored() {
        let m = vec![OWN.to_string()];
        assert_eq!(classify(&facts(false, false, &m)), Verdict::Ignore(Ignore::OtherRoom));
        let own = Facts {
            sender: "@Me:dev.matrix.inblock.io",
            ..facts(true, false, &m)
        };
        assert_eq!(classify(&own), Verdict::Ignore(Ignore::Own));
    }
}
