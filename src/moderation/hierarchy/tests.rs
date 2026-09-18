use super::*;

fn guild() -> Guild {
    serde_json::from_value(super::super::tests::hierarchy_guild(100, 900)).unwrap()
}

#[test]
fn strikes_require_a_strictly_lower_highest_role() {
    let guild = guild();
    let bot_roles = [Id::new(11)];
    for (roles, allowed) in [
        (vec![], true),
        (vec![Id::new(12)], true),
        (vec![Id::new(11)], false),
        (vec![Id::new(13)], false),
        (vec![Id::new(12), Id::new(13)], false),
    ] {
        assert_eq!(
            below_bot(&guild, Id::new(600), &bot_roles, Id::new(300), &roles).unwrap(),
            allowed
        );
    }
    for protected in [600, 900] {
        assert!(!below_bot(&guild, Id::new(600), &bot_roles, Id::new(protected), &[]).unwrap());
    }
    assert!(!below_bot(&guild, Id::new(600), &[], Id::new(300), &[]).unwrap());
}

#[test]
fn role_order_uses_discord_ties_and_everyone_is_always_lowest() {
    let mut guild = guild();
    guild.roles.iter_mut().for_each(|role| role.position = 0);
    // Same position: role 11 outranks 12, but is below role 10.
    let mut higher = guild.roles[1].clone();
    higher.id = Id::new(10);
    guild.roles.push(higher);
    assert!(
        below_bot(
            &guild,
            Id::new(600),
            &[Id::new(11)],
            Id::new(300),
            &[Id::new(12)]
        )
        .unwrap()
    );
    assert!(
        !below_bot(
            &guild,
            Id::new(600),
            &[Id::new(11)],
            Id::new(300),
            &[Id::new(10)]
        )
        .unwrap()
    );
    // A custom role even with a negative position still outranks @everyone.
    guild
        .roles
        .iter_mut()
        .filter(|role| role.id.get() != 100)
        .for_each(|role| role.position = -1);
    assert!(below_bot(&guild, Id::new(600), &[Id::new(11)], Id::new(300), &[]).unwrap());
    assert!(!below_bot(&guild, Id::new(600), &[], Id::new(300), &[Id::new(11)]).unwrap());
}

#[test]
fn incomplete_role_data_never_assumes_a_lower_rank() {
    let mut guild = guild();
    for (bot, target) in [
        (vec![Id::new(99)], vec![]),
        (vec![Id::new(11)], vec![Id::new(99)]),
    ] {
        assert!(below_bot(&guild, Id::new(600), &bot, Id::new(300), &target).is_err());
    }
    guild.roles.retain(|role| role.id.get() != 100);
    assert!(below_bot(&guild, Id::new(600), &[Id::new(11)], Id::new(300), &[]).is_err());
}
