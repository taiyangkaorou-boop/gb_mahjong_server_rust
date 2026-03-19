// 这些是 room 模块共享的纯函数和构造器。
// 它们不直接持有状态，拆出来后能减少根文件体积，也便于后续提炼成更稳定的工具层。
// 这一段放不依赖 RoomState 可变借用布局的纯辅助函数，
// 主要负责帧构造、牌墙生成、枚举映射和一些通用小工具。
fn build_join_room_response(
    event_seq: u64,
    request_id: String,
    accepted: bool,
    reject_code: RejectCode,
    room_id: String,
    match_id: String,
    seat: Seat,
    connection_id: String,
    resume_token: String,
    room_config: Option<RoomConfig>,
    message: impl Into<String>,
) -> ServerFrame {
    ServerFrame {
        event_seq,
        payload: Some(server_frame::Payload::JoinRoom(JoinRoomResponse {
            request_id,
            accepted,
            reject_code: reject_code as i32,
            room_id,
            match_id,
            seat: seat as i32,
            connection_id,
            resume_token,
            room_config,
            message: message.into(),
            latest_event_seq: event_seq,
        })),
    }
}

fn build_player_connection_changed(
    event_seq: u64,
    room_id: String,
    match_id: String,
    user_id: String,
    seat: Seat,
    status: PlayerStatus,
    connected: bool,
) -> ServerFrame {
    ServerFrame {
        event_seq,
        payload: Some(server_frame::Payload::PlayerConnectionChanged(
            PlayerConnectionChanged {
                room_id,
                match_id,
                user_id,
                seat: seat as i32,
                status: status as i32,
                connected,
                reconnect_deadline_unix_ms: if connected {
                    0
                } else {
                    unix_time_ms() + RECONNECT_GRACE_MS
                },
            },
        )),
    }
}

fn build_action_rejected(
    event_seq: u64,
    request_id: String,
    reject_code: RejectCode,
    message: impl Into<String>,
    expected_event_seq: u64,
    action_window_id: u64,
) -> ServerFrame {
    ServerFrame {
        event_seq,
        payload: Some(server_frame::Payload::ActionRejected(ActionRejected {
            request_id,
            reject_code: reject_code as i32,
            message: message.into(),
            expected_event_seq,
            actual_event_seq: event_seq,
            action_window_id,
        })),
    }
}

fn unix_time_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

fn build_shuffled_wall(room_config: &RoomConfig, room_id: &str, hand_number: u32) -> Vec<Tile> {
    let mut wall = build_standard_wall(room_config.enable_flower_tiles);
    let mut seed = stable_wall_seed(room_id, hand_number);

    // 这里用轻量 xorshift 做无依赖洗牌。
    // 测试环境会通过注入 wall_factory 使用固定牌墙。
    for index in (1..wall.len()).rev() {
        seed = xorshift64(seed);
        let swap_index = (seed as usize) % (index + 1);
        wall.swap(index, swap_index);
    }

    wall
}

fn build_standard_wall(enable_flower_tiles: bool) -> Vec<Tile> {
    let mut wall = Vec::new();

    for tile in [
        Tile::Character1,
        Tile::Character2,
        Tile::Character3,
        Tile::Character4,
        Tile::Character5,
        Tile::Character6,
        Tile::Character7,
        Tile::Character8,
        Tile::Character9,
        Tile::Bamboo1,
        Tile::Bamboo2,
        Tile::Bamboo3,
        Tile::Bamboo4,
        Tile::Bamboo5,
        Tile::Bamboo6,
        Tile::Bamboo7,
        Tile::Bamboo8,
        Tile::Bamboo9,
        Tile::Dot1,
        Tile::Dot2,
        Tile::Dot3,
        Tile::Dot4,
        Tile::Dot5,
        Tile::Dot6,
        Tile::Dot7,
        Tile::Dot8,
        Tile::Dot9,
        Tile::EastWind,
        Tile::SouthWind,
        Tile::WestWind,
        Tile::NorthWind,
        Tile::RedDragon,
        Tile::GreenDragon,
        Tile::WhiteDragon,
    ] {
        for _ in 0..4 {
            wall.push(tile);
        }
    }

    if enable_flower_tiles {
        wall.extend([
            Tile::FlowerPlum,
            Tile::FlowerOrchid,
            Tile::FlowerBamboo,
            Tile::FlowerChrysanthemum,
            Tile::SeasonSpring,
            Tile::SeasonSummer,
            Tile::SeasonAutumn,
            Tile::SeasonWinter,
        ]);
    }

    wall
}

fn stable_wall_seed(room_id: &str, hand_number: u32) -> u64 {
    let mut hash = 1469598103934665603_u64;
    for byte in room_id.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(1099511628211);
    }

    hash ^ (u64::from(hand_number) << 32) ^ unix_time_ms()
}

fn xorshift64(mut state: u64) -> u64 {
    if state == 0 {
        state = 0x9e37_79b9_7f4a_7c15;
    }
    state ^= state << 13;
    state ^= state >> 7;
    state ^= state << 17;
    state
}

fn is_flower_tile(tile: Tile) -> bool {
    matches!(
        tile,
        Tile::FlowerPlum
            | Tile::FlowerOrchid
            | Tile::FlowerBamboo
            | Tile::FlowerChrysanthemum
            | Tile::SeasonSpring
            | Tile::SeasonSummer
            | Tile::SeasonAutumn
            | Tile::SeasonWinter
    )
}

fn sort_tiles(tiles: &mut Vec<Tile>) {
    tiles.sort_by_key(|tile| *tile as i32);
}

fn private_tile_count(round_state: &PlayerRoundState) -> usize {
    round_state.concealed_tiles.len() + usize::from(round_state.drawn_tile.is_some())
}

fn merged_private_tiles(round_state: &PlayerRoundState) -> Vec<Tile> {
    let mut tiles = round_state.concealed_tiles.clone();
    if let Some(drawn_tile) = round_state.drawn_tile {
        tiles.push(drawn_tile);
    }
    sort_tiles(&mut tiles);
    tiles
}

fn has_n_tiles(tiles: &[Tile], target_tile: Tile, required_count: usize) -> bool {
    tiles.iter().filter(|tile| **tile == target_tile).count() >= required_count
}

fn build_chi_consume_patterns(concealed_tiles: &[Tile], target_tile: Tile) -> Vec<Vec<Tile>> {
    let Some((suit, rank)) = tile_suit_and_rank(target_tile) else {
        return Vec::new();
    };

    let mut patterns = Vec::new();
    for (left_rank, right_rank) in [(rank - 2, rank - 1), (rank - 1, rank + 1), (rank + 1, rank + 2)] {
        if !(1..=9).contains(&left_rank) || !(1..=9).contains(&right_rank) {
            continue;
        }

        let left_tile = tile_from_suit_and_rank(suit, left_rank);
        let right_tile = tile_from_suit_and_rank(suit, right_rank);
        if has_n_tiles(concealed_tiles, left_tile, 1) && has_n_tiles(concealed_tiles, right_tile, 1) {
            let mut consume_tiles = vec![left_tile, right_tile];
            sort_tiles(&mut consume_tiles);
            if !patterns.contains(&consume_tiles) {
                patterns.push(consume_tiles);
            }
        }
    }

    patterns
}

fn remove_tiles_from_concealed(
    concealed_tiles: &mut Vec<Tile>,
    consume_tiles: &[Tile],
) -> Result<(), ActionValidationError> {
    for tile in consume_tiles {
        let Some(position) = concealed_tiles.iter().position(|hand_tile| hand_tile == tile) else {
            return Err(ActionValidationError::new(
                RejectCode::InvalidTile,
                "claim consume_tiles are not all present in the player's concealed hand",
            ));
        };
        concealed_tiles.remove(position);
    }
    sort_tiles(concealed_tiles);
    Ok(())
}

fn consume_tile_for_supplemental_kong(
    round_state: &mut PlayerRoundState,
    tile: Tile,
) -> Result<(), ActionValidationError> {
    if round_state.drawn_tile == Some(tile) {
        round_state.drawn_tile = None;
        return Ok(());
    }

    let Some(position) = round_state
        .concealed_tiles
        .iter()
        .position(|hand_tile| *hand_tile == tile)
    else {
        return Err(ActionValidationError::new(
            RejectCode::InvalidTile,
            "supplemental kong tile is not present in concealed tiles or drawn tile",
        ));
    };

    round_state.concealed_tiles.remove(position);
    if let Some(drawn_tile) = round_state.drawn_tile.take() {
        round_state.concealed_tiles.push(drawn_tile);
        sort_tiles(&mut round_state.concealed_tiles);
    }

    Ok(())
}

fn next_seat(seat: Seat) -> Seat {
    match seat {
        Seat::East => Seat::South,
        Seat::South => Seat::West,
        Seat::West => Seat::North,
        Seat::North | Seat::Unspecified => Seat::East,
    }
}

fn map_seat_to_engine(seat: Seat) -> engine_proto::Seat {
    engine_proto::Seat::from_str_name(seat.as_str_name()).unwrap_or(engine_proto::Seat::Unspecified)
}

fn map_engine_seat_to_client(seat: i32) -> Option<Seat> {
    let engine_seat = engine_proto::Seat::try_from(seat).ok()?;
    Seat::from_str_name(engine_seat.as_str_name())
}

fn map_action_kind_to_engine(
    action_kind: crate::proto::client::ActionKind,
) -> engine_proto::ActionKind {
    engine_proto::ActionKind::from_str_name(action_kind.as_str_name())
        .unwrap_or(engine_proto::ActionKind::Unspecified)
}

fn map_validation_reject_code(reject_code: i32) -> RejectCode {
    match engine_proto::ValidationRejectCode::try_from(reject_code)
        .unwrap_or(engine_proto::ValidationRejectCode::Unspecified)
    {
        engine_proto::ValidationRejectCode::Unspecified => RejectCode::RuleValidationFailed,
        engine_proto::ValidationRejectCode::InvalidContext => RejectCode::InvalidRequest,
        engine_proto::ValidationRejectCode::TileNotOwned
        | engine_proto::ValidationRejectCode::TileNotAvailable => RejectCode::InvalidTile,
        engine_proto::ValidationRejectCode::MeldNotFormable
        | engine_proto::ValidationRejectCode::HandNotComplete
        | engine_proto::ValidationRejectCode::RulesetViolation => RejectCode::RuleValidationFailed,
        engine_proto::ValidationRejectCode::ActionWindowMismatch => RejectCode::ActionWindowClosed,
        engine_proto::ValidationRejectCode::InternalError => RejectCode::InternalError,
    }
}

fn parse_client_seat_value(seat: i32) -> Result<i32, ActionValidationError> {
    Ok(map_seat_to_engine(parse_client_seat(seat)?) as i32)
}

fn parse_client_tile_value(tile: i32) -> Result<i32, ActionValidationError> {
    Ok(map_tile_to_engine(parse_client_tile(tile)?) as i32)
}

fn parse_client_claim_kind_value(claim_kind: i32) -> Result<i32, ActionValidationError> {
    Ok(map_claim_kind_to_engine(parse_client_claim_kind(claim_kind)?) as i32)
}

fn parse_client_win_type_value(win_type: i32) -> Result<i32, ActionValidationError> {
    Ok(map_win_type_to_engine(parse_client_win_type(win_type)?) as i32)
}

fn parse_client_seat(seat: i32) -> Result<Seat, ActionValidationError> {
    Seat::try_from(seat).map_err(|_| {
        ActionValidationError::new(
            RejectCode::InvalidRequest,
            format!("unknown seat enum value {seat}"),
        )
    })
}

fn parse_client_tile(tile: i32) -> Result<Tile, ActionValidationError> {
    Tile::try_from(tile).map_err(|_| {
        ActionValidationError::new(
            RejectCode::InvalidTile,
            format!("unknown tile enum value {tile}"),
        )
    })
}

fn parse_client_claim_kind(
    claim_kind: i32,
) -> Result<crate::proto::client::ClaimKind, ActionValidationError> {
    crate::proto::client::ClaimKind::try_from(claim_kind).map_err(|_| {
        ActionValidationError::new(
            RejectCode::InvalidRequest,
            format!("unknown claim kind enum value {claim_kind}"),
        )
    })
}

fn parse_client_win_type(win_type: i32) -> Result<crate::proto::client::WinType, ActionValidationError> {
    crate::proto::client::WinType::try_from(win_type).map_err(|_| {
        ActionValidationError::new(
            RejectCode::InvalidRequest,
            format!("unknown win type enum value {win_type}"),
        )
    })
}

fn map_tile_to_engine(tile: Tile) -> engine_proto::Tile {
    engine_proto::Tile::from_str_name(tile.as_str_name()).unwrap_or(engine_proto::Tile::Unspecified)
}

fn map_meld_kind_to_engine(meld_kind: crate::proto::client::MeldKind) -> engine_proto::MeldKind {
    engine_proto::MeldKind::from_str_name(meld_kind.as_str_name())
        .unwrap_or(engine_proto::MeldKind::Unspecified)
}

fn map_client_meld_to_engine(meld: &Meld) -> engine_proto::Meld {
    engine_proto::Meld {
        kind: map_meld_kind_to_engine(
            crate::proto::client::MeldKind::try_from(meld.kind)
                .unwrap_or(crate::proto::client::MeldKind::Unspecified),
        ) as i32,
        tiles: meld
            .tiles
            .iter()
            .copied()
            .map(parse_client_tile)
            .filter_map(Result::ok)
            .map(|tile| map_tile_to_engine(tile) as i32)
            .collect(),
        from_seat: parse_client_seat_value(meld.from_seat)
            .unwrap_or(engine_proto::Seat::Unspecified as i32),
        claimed_tile: parse_client_tile_value(meld.claimed_tile)
            .unwrap_or(engine_proto::Tile::Unspecified as i32),
        concealed: meld.concealed,
        source_event_seq: meld.source_event_seq,
    }
}

fn map_client_discard_to_engine(
    discard: &crate::proto::client::DiscardTile,
) -> engine_proto::DiscardTile {
    engine_proto::DiscardTile {
        tile: parse_client_tile_value(discard.tile)
            .unwrap_or(engine_proto::Tile::Unspecified as i32),
        turn_index: discard.turn_index,
        claimed: discard.claimed,
        drawn_and_discarded: discard.drawn_and_discarded,
        source_event_seq: discard.source_event_seq,
    }
}

fn map_claim_kind_to_engine(
    claim_kind: crate::proto::client::ClaimKind,
) -> engine_proto::ClaimKind {
    engine_proto::ClaimKind::from_str_name(claim_kind.as_str_name())
        .unwrap_or(engine_proto::ClaimKind::Unspecified)
}

fn map_claim_kind_to_client_action(
    claim_kind: crate::proto::client::ClaimKind,
) -> crate::proto::client::ActionKind {
    match claim_kind {
        crate::proto::client::ClaimKind::Chi => crate::proto::client::ActionKind::Chi,
        crate::proto::client::ClaimKind::Peng => crate::proto::client::ActionKind::Peng,
        crate::proto::client::ClaimKind::ExposedKong => {
            crate::proto::client::ActionKind::ExposedKong
        }
        crate::proto::client::ClaimKind::DiscardWin
        | crate::proto::client::ClaimKind::RobKongWin => {
            crate::proto::client::ActionKind::DeclareWin
        }
        crate::proto::client::ClaimKind::Unspecified => {
            crate::proto::client::ActionKind::Unspecified
        }
    }
}

fn map_claim_kind_to_meld_kind(
    claim_kind: crate::proto::client::ClaimKind,
) -> crate::proto::client::MeldKind {
    match claim_kind {
        crate::proto::client::ClaimKind::Chi => crate::proto::client::MeldKind::Chi,
        crate::proto::client::ClaimKind::Peng => crate::proto::client::MeldKind::Peng,
        crate::proto::client::ClaimKind::ExposedKong => {
            crate::proto::client::MeldKind::ExposedKong
        }
        crate::proto::client::ClaimKind::DiscardWin
        | crate::proto::client::ClaimKind::RobKongWin
        | crate::proto::client::ClaimKind::Unspecified => {
            crate::proto::client::MeldKind::Unspecified
        }
    }
}

fn map_win_type_to_engine(win_type: crate::proto::client::WinType) -> engine_proto::WinType {
    engine_proto::WinType::from_str_name(win_type.as_str_name())
        .unwrap_or(engine_proto::WinType::Unspecified)
}

fn map_engine_settlement_flag_to_client(flag: i32) -> Option<crate::proto::client::SettlementFlag> {
    let engine_flag = engine_proto::SettlementFlag::try_from(flag).ok()?;
    crate::proto::client::SettlementFlag::from_str_name(engine_flag.as_str_name())
}

fn claim_response_priority(response: &ClaimResponse) -> Option<u8> {
    match response {
        ClaimResponse::Pass => None,
        ClaimResponse::DeclareWin(_) => Some(3),
        ClaimResponse::Claim(claim) => match parse_client_claim_kind(claim.claim_kind).ok()? {
            crate::proto::client::ClaimKind::Chi => Some(1),
            crate::proto::client::ClaimKind::Peng
            | crate::proto::client::ClaimKind::ExposedKong => Some(2),
            crate::proto::client::ClaimKind::DiscardWin
            | crate::proto::client::ClaimKind::RobKongWin => Some(3),
            crate::proto::client::ClaimKind::Unspecified => None,
        },
    }
}

fn claim_priority_distance(source_seat: Seat, candidate_seat: Seat) -> usize {
    let mut current_seat = source_seat;
    for distance in 1..=SEAT_ORDER.len() {
        current_seat = next_seat(current_seat);
        if current_seat == candidate_seat {
            return distance;
        }
    }

    SEAT_ORDER.len() + 1
}

fn tile_suit_and_rank(tile: Tile) -> Option<(&'static str, i32)> {
    match tile {
        Tile::Character1 => Some(("character", 1)),
        Tile::Character2 => Some(("character", 2)),
        Tile::Character3 => Some(("character", 3)),
        Tile::Character4 => Some(("character", 4)),
        Tile::Character5 => Some(("character", 5)),
        Tile::Character6 => Some(("character", 6)),
        Tile::Character7 => Some(("character", 7)),
        Tile::Character8 => Some(("character", 8)),
        Tile::Character9 => Some(("character", 9)),
        Tile::Bamboo1 => Some(("bamboo", 1)),
        Tile::Bamboo2 => Some(("bamboo", 2)),
        Tile::Bamboo3 => Some(("bamboo", 3)),
        Tile::Bamboo4 => Some(("bamboo", 4)),
        Tile::Bamboo5 => Some(("bamboo", 5)),
        Tile::Bamboo6 => Some(("bamboo", 6)),
        Tile::Bamboo7 => Some(("bamboo", 7)),
        Tile::Bamboo8 => Some(("bamboo", 8)),
        Tile::Bamboo9 => Some(("bamboo", 9)),
        Tile::Dot1 => Some(("dot", 1)),
        Tile::Dot2 => Some(("dot", 2)),
        Tile::Dot3 => Some(("dot", 3)),
        Tile::Dot4 => Some(("dot", 4)),
        Tile::Dot5 => Some(("dot", 5)),
        Tile::Dot6 => Some(("dot", 6)),
        Tile::Dot7 => Some(("dot", 7)),
        Tile::Dot8 => Some(("dot", 8)),
        Tile::Dot9 => Some(("dot", 9)),
        _ => None,
    }
}

fn tile_from_suit_and_rank(suit: &str, rank: i32) -> Tile {
    match (suit, rank) {
        ("character", 1) => Tile::Character1,
        ("character", 2) => Tile::Character2,
        ("character", 3) => Tile::Character3,
        ("character", 4) => Tile::Character4,
        ("character", 5) => Tile::Character5,
        ("character", 6) => Tile::Character6,
        ("character", 7) => Tile::Character7,
        ("character", 8) => Tile::Character8,
        ("character", 9) => Tile::Character9,
        ("bamboo", 1) => Tile::Bamboo1,
        ("bamboo", 2) => Tile::Bamboo2,
        ("bamboo", 3) => Tile::Bamboo3,
        ("bamboo", 4) => Tile::Bamboo4,
        ("bamboo", 5) => Tile::Bamboo5,
        ("bamboo", 6) => Tile::Bamboo6,
        ("bamboo", 7) => Tile::Bamboo7,
        ("bamboo", 8) => Tile::Bamboo8,
        ("bamboo", 9) => Tile::Bamboo9,
        ("dot", 1) => Tile::Dot1,
        ("dot", 2) => Tile::Dot2,
        ("dot", 3) => Tile::Dot3,
        ("dot", 4) => Tile::Dot4,
        ("dot", 5) => Tile::Dot5,
        ("dot", 6) => Tile::Dot6,
        ("dot", 7) => Tile::Dot7,
        ("dot", 8) => Tile::Dot8,
        ("dot", 9) => Tile::Dot9,
        _ => Tile::Unspecified,
    }
}
