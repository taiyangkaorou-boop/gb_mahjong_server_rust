#include "gb_mahjong_adapter.h"

#include <algorithm>
#include <array>
#include <cstdint>
#include <stdexcept>
#include <string>
#include <utility>
#include <vector>

#include "fan.h"
#include "handtiles.h"
#include "pack.h"
#include "tile.h"

namespace proto = gb_mahjong::engine::v1;

namespace gb_mahjong::engine_server {

namespace {

bool IsBlank(const std::string& value) {
  return value.empty();
}

proto::Seat FindActorSeat(
    const proto::ValidateActionRequest& request,
    const proto::EnginePlayerState** actor_state) {
  for (const auto& player : request.players()) {
    if (player.seat() == request.actor_seat()) {
      if (actor_state != nullptr) {
        *actor_state = &player;
      }
      return player.seat();
    }
  }

  if (actor_state != nullptr) {
    *actor_state = nullptr;
  }
  return request.actor_seat();
}

int SeatToIndex(proto::Seat seat) {
  switch (seat) {
    case proto::SEAT_EAST:
      return 0;
    case proto::SEAT_SOUTH:
      return 1;
    case proto::SEAT_WEST:
      return 2;
    case proto::SEAT_NORTH:
      return 3;
    case proto::SEAT_UNSPECIFIED:
    default:
      throw std::invalid_argument("seat must not be unspecified");
  }
}

int ProtoSeatToMahjongWind(proto::Seat seat) {
  switch (seat) {
    case proto::SEAT_EAST:
      return WIND_E;
    case proto::SEAT_SOUTH:
      return WIND_S;
    case proto::SEAT_WEST:
      return WIND_W;
    case proto::SEAT_NORTH:
      return WIND_N;
    case proto::SEAT_UNSPECIFIED:
    default:
      throw std::invalid_argument("seat must not be unspecified");
  }
}

mahjong::Tile ProtoTileToMahjongTile(proto::Tile tile) {
  if (tile == proto::TILE_UNSPECIFIED || tile == proto::TILE_UNKNOWN) {
    throw std::invalid_argument("tile must not be unspecified or unknown");
  }

  return mahjong::Tile(static_cast<int>(tile) - 1);
}

int RelativeOffer(proto::Seat actor_seat, proto::Seat from_seat) {
  const int diff =
      (SeatToIndex(actor_seat) - SeatToIndex(from_seat) + 4) % 4;
  if (diff == 0) {
    throw std::invalid_argument("from_seat must differ from actor seat");
  }
  return diff;
}

bool IsWinningClaimKind(proto::ClaimKind claim_kind) {
  return claim_kind == proto::CLAIM_KIND_DISCARD_WIN ||
         claim_kind == proto::CLAIM_KIND_ROB_KONG_WIN;
}

bool IsWinningAction(const proto::CandidateAction& candidate_action) {
  if (candidate_action.action_case() == proto::CandidateAction::kDeclareWin) {
    return true;
  }

  return candidate_action.action_case() == proto::CandidateAction::kClaim &&
         IsWinningClaimKind(candidate_action.claim().claim_kind());
}

proto::WinType ExtractWinType(const proto::CandidateAction& candidate_action) {
  switch (candidate_action.action_case()) {
    case proto::CandidateAction::kDeclareWin:
      return candidate_action.declare_win().win_type();
    case proto::CandidateAction::kClaim:
      if (candidate_action.claim().claim_kind() ==
          proto::CLAIM_KIND_ROB_KONG_WIN) {
        return proto::WIN_TYPE_ROB_KONG;
      }
      if (candidate_action.claim().claim_kind() ==
          proto::CLAIM_KIND_DISCARD_WIN) {
        return proto::WIN_TYPE_DISCARD;
      }
      break;
    case proto::CandidateAction::ACTION_NOT_SET:
    case proto::CandidateAction::kDiscard:
    case proto::CandidateAction::kPass:
    case proto::CandidateAction::kKong:
    case proto::CandidateAction::kReplaceFlower:
      break;
  }

  throw std::invalid_argument("candidate action is not a winning action");
}

proto::Tile ExtractWinningTile(const proto::CandidateAction& candidate_action) {
  switch (candidate_action.action_case()) {
    case proto::CandidateAction::kDeclareWin:
      return candidate_action.declare_win().winning_tile();
    case proto::CandidateAction::kClaim:
      return candidate_action.claim().target_tile();
    case proto::CandidateAction::ACTION_NOT_SET:
    case proto::CandidateAction::kDiscard:
    case proto::CandidateAction::kPass:
    case proto::CandidateAction::kKong:
    case proto::CandidateAction::kReplaceFlower:
      break;
  }

  throw std::invalid_argument("candidate action does not carry a winning tile");
}

void SeedHandtilesTables(mahjong::Handtiles* handtiles) {
  for (int tile = TILE_1m; tile < TILE_SIZE; ++tile) {
    handtiles->fulu_table[tile] = 0;
    handtiles->lipai_table[tile] = 0;
  }

  for (int tile = TILE_MEI; tile <= TILE_DONG; ++tile) {
    handtiles->huapai_table[tile] = 0;
    handtiles->lipai_table[tile] = 0;
  }
}

void AddFlower(mahjong::Handtiles* handtiles, proto::Tile tile) {
  mahjong::Tile mahjong_tile = ProtoTileToMahjongTile(tile);
  handtiles->huapai.push_back(mahjong_tile);
  handtiles->huapai_table[mahjong_tile.GetId()]++;
}

mahjong::Pack ProtoMeldToMahjongPack(const proto::Meld& meld,
                                     proto::Seat actor_seat) {
  const proto::MeldKind meld_kind = meld.kind();
  const proto::Tile claimed_tile = meld.claimed_tile();

  if (meld.tiles_size() == 0) {
    throw std::invalid_argument("meld.tiles must not be empty");
  }

  std::vector<mahjong::Tile> tiles;
  tiles.reserve(meld.tiles_size());
  for (int tile : meld.tiles()) {
    tiles.push_back(ProtoTileToMahjongTile(static_cast<proto::Tile>(tile)));
  }
  std::sort(tiles.begin(), tiles.end());

  switch (meld_kind) {
    case proto::MELD_KIND_CHI: {
      if (tiles.size() != 3) {
        throw std::invalid_argument("chi meld must contain exactly 3 tiles");
      }

      const mahjong::Tile claimed = ProtoTileToMahjongTile(claimed_tile);
      int offer = 0;
      if (tiles[0] == claimed) {
        offer = 1;
      } else if (tiles[1] == claimed) {
        offer = 2;
      } else if (tiles[2] == claimed) {
        offer = 3;
      } else {
        throw std::invalid_argument("claimed tile must belong to chi meld");
      }

      return mahjong::Pack(
          PACK_TYPE_SHUNZI,
          tiles[1],
          0,
          offer);
    }
    case proto::MELD_KIND_PENG:
      return mahjong::Pack(
          PACK_TYPE_KEZI,
          tiles.front(),
          0,
          meld.concealed() ? 0 : RelativeOffer(actor_seat, meld.from_seat()));
    case proto::MELD_KIND_EXPOSED_KONG:
      return mahjong::Pack(
          PACK_TYPE_GANG,
          tiles.front(),
          0,
          RelativeOffer(actor_seat, meld.from_seat()));
    case proto::MELD_KIND_CONCEALED_KONG:
      return mahjong::Pack(PACK_TYPE_GANG, tiles.front(), 0, 0);
    case proto::MELD_KIND_SUPPLEMENTAL_KONG: {
      const int base_offer =
          meld.concealed() ? 0 : RelativeOffer(actor_seat, meld.from_seat());
      const int offer = base_offer == 0 ? 0 : base_offer + 4;
      return mahjong::Pack(PACK_TYPE_GANG, tiles.front(), 0, offer);
    }
    case proto::MELD_KIND_UNSPECIFIED:
    default:
      throw std::invalid_argument("meld kind must not be unspecified");
  }
}

void AddMeld(mahjong::Handtiles* handtiles, const proto::Meld& meld,
             proto::Seat actor_seat) {
  mahjong::Pack pack = ProtoMeldToMahjongPack(meld, actor_seat);
  handtiles->fulu.push_back(pack);
  for (const auto& tile : pack.GetAllTile()) {
    handtiles->fulu_table[tile.GetId()]++;
  }
}

void AddLipaiTile(mahjong::Handtiles* handtiles, const mahjong::Tile& tile) {
  handtiles->lipai.push_back(tile);
  handtiles->lipai_table[tile.GetId()]++;
}

bool ContainsSettlementFlag(const proto::CalculateScoreRequest& request,
                            proto::SettlementFlag flag) {
  for (int settlement_flag : request.settlement_flags()) {
    if (settlement_flag == flag) {
      return true;
    }
  }
  return false;
}

void PopulateFanDetails(const mahjong::Fan& fan,
                        std::vector<proto::FanDetail>* fan_details) {
  for (int fan_index = 1; fan_index < mahjong::FAN_SIZE; ++fan_index) {
    const auto& entries = fan.fan_table_res[fan_index];
    if (entries.empty()) {
      continue;
    }

    proto::FanDetail detail;
    detail.set_fan_code("gbm_fan_" + std::to_string(fan_index));
    detail.set_fan_name(mahjong::FAN_NAME[fan_index]);
    detail.set_fan_value(mahjong::FAN_SCORE[fan_index]);
    detail.set_count(static_cast<uint32_t>(entries.size()));
    detail.set_description(
        std::string("由 GB-Mahjong 识别出的番种，出现次数为 ") +
        std::to_string(entries.size()));
    fan_details->push_back(std::move(detail));
  }
}

void PopulateFanDetails(
    const mahjong::Fan& fan,
    google::protobuf::RepeatedPtrField<proto::FanDetail>* fan_details) {
  for (int fan_index = 1; fan_index < mahjong::FAN_SIZE; ++fan_index) {
    const auto& entries = fan.fan_table_res[fan_index];
    if (entries.empty()) {
      continue;
    }

    proto::FanDetail detail;
    detail.set_fan_code("gbm_fan_" + std::to_string(fan_index));
    detail.set_fan_name(mahjong::FAN_NAME[fan_index]);
    detail.set_fan_value(mahjong::FAN_SCORE[fan_index]);
    detail.set_count(static_cast<uint32_t>(entries.size()));
    detail.set_description(
        std::string("由 GB-Mahjong 识别出的番种，出现次数为 ") +
        std::to_string(entries.size()));
    *fan_details->Add() = std::move(detail);
  }
}

bool HasFan(const mahjong::Fan& fan, int fan_index) {
  return !fan.fan_table_res[fan_index].empty();
}

mahjong::Handtiles BuildWinningHandtilesForValidation(
    const proto::ValidateActionRequest& request) {
  const proto::EnginePlayerState* actor_state = nullptr;
  FindActorSeat(request, &actor_state);
  if (actor_state == nullptr) {
    throw std::invalid_argument("players must contain the actor seat state");
  }

  if (!request.has_actor_private_hand()) {
    throw std::invalid_argument("actor_private_hand must be present");
  }

  mahjong::Handtiles handtiles;
  SeedHandtilesTables(&handtiles);
  handtiles.SetQuanfeng(
      ProtoSeatToMahjongWind(request.round_context().prevailing_wind()));
  handtiles.SetMenfeng(ProtoSeatToMahjongWind(request.actor_seat()));

  const proto::CandidateAction& candidate_action = request.candidate_action();
  const proto::WinType win_type = ExtractWinType(candidate_action);
  handtiles.SetZimo(win_type == proto::WIN_TYPE_SELF_DRAW ||
                    win_type == proto::WIN_TYPE_KONG_DRAW);
  handtiles.SetJuezhang(0);
  handtiles.SetHaidi(request.round_context().is_last_tile() ? 1 : 0);
  handtiles.SetGang(win_type == proto::WIN_TYPE_ROB_KONG ||
                    win_type == proto::WIN_TYPE_KONG_DRAW);

  for (const auto& meld : actor_state->melds()) {
    AddMeld(&handtiles, meld, request.actor_seat());
  }

  for (int flower : actor_state->flowers()) {
    AddFlower(&handtiles, static_cast<proto::Tile>(flower));
  }

  for (int concealed_tile : request.actor_private_hand().concealed_tiles()) {
    AddLipaiTile(
        &handtiles,
        ProtoTileToMahjongTile(static_cast<proto::Tile>(concealed_tile)));
  }

  const proto::Tile last_tile =
      win_type == proto::WIN_TYPE_SELF_DRAW || win_type == proto::WIN_TYPE_KONG_DRAW
          ? request.actor_private_hand().drawn_tile()
          : ExtractWinningTile(candidate_action);
  AddLipaiTile(&handtiles, ProtoTileToMahjongTile(last_tile));

  if (handtiles.fulu.size() * 3 + handtiles.lipai.size() != 14) {
    throw std::invalid_argument(
        "winning validation requires a complete 14-tile hand snapshot");
  }

  if (handtiles.IsZimo()) {
    handtiles.LastLipai().SetZimo();
  } else {
    handtiles.LastLipai().SetChonghu();
  }
  handtiles.SortLipaiWithoutLastOne();
  return handtiles;
}

const proto::HandSnapshot& FindWinnerHand(
    const proto::CalculateScoreRequest& request) {
  for (const auto& hand : request.hands()) {
    if (hand.winner() || hand.seat() == request.winner_seat()) {
      return hand;
    }
  }

  throw std::invalid_argument(
      "CalculateScoreRequest must include the winner hand snapshot");
}

void MoveOneTileToBack(std::vector<mahjong::Tile>* tiles,
                       const mahjong::Tile& tile) {
  auto it = std::find(tiles->begin(), tiles->end(), tile);
  if (it == tiles->end()) {
    throw std::invalid_argument("winning tile is not present in concealed tiles");
  }

  mahjong::Tile selected = *it;
  tiles->erase(it);
  tiles->push_back(selected);
}

mahjong::Handtiles BuildWinningHandtilesForScoring(
    const proto::CalculateScoreRequest& request, const proto::HandSnapshot& hand) {
  mahjong::Handtiles handtiles;
  SeedHandtilesTables(&handtiles);
  handtiles.SetQuanfeng(
      ProtoSeatToMahjongWind(request.round_context().prevailing_wind()));
  handtiles.SetMenfeng(ProtoSeatToMahjongWind(hand.seat()));
  handtiles.SetZimo(request.win_type() == proto::WIN_TYPE_SELF_DRAW ||
                    request.win_type() == proto::WIN_TYPE_KONG_DRAW);
  handtiles.SetJuezhang(0);
  handtiles.SetHaidi(
      request.round_context().is_last_tile() ||
      ContainsSettlementFlag(request, proto::SETTLEMENT_FLAG_LAST_TILE_DRAW) ||
      ContainsSettlementFlag(request, proto::SETTLEMENT_FLAG_LAST_TILE_CLAIM));
  handtiles.SetGang(request.win_type() == proto::WIN_TYPE_ROB_KONG ||
                    request.win_type() == proto::WIN_TYPE_KONG_DRAW);

  for (const auto& meld : hand.melds()) {
    AddMeld(&handtiles, meld, hand.seat());
  }

  for (int flower : hand.flowers()) {
    AddFlower(&handtiles, static_cast<proto::Tile>(flower));
  }

  std::vector<mahjong::Tile> concealed_tiles;
  concealed_tiles.reserve(hand.concealed_tiles_size() + 1);
  for (int concealed_tile : hand.concealed_tiles()) {
    concealed_tiles.push_back(
        ProtoTileToMahjongTile(static_cast<proto::Tile>(concealed_tile)));
  }

  const int visible_tile_count =
      static_cast<int>(hand.melds_size()) * 3 + hand.concealed_tiles_size();
  const mahjong::Tile winning_tile =
      ProtoTileToMahjongTile(request.winning_tile());

  if (visible_tile_count == 13) {
    concealed_tiles.push_back(winning_tile);
  } else if (visible_tile_count == 14) {
    MoveOneTileToBack(&concealed_tiles, winning_tile);
  } else {
    throw std::invalid_argument(
        "winner hand snapshot must contain either 13 tiles plus winning tile or a full 14 tiles");
  }

  for (const auto& tile : concealed_tiles) {
    AddLipaiTile(&handtiles, tile);
  }

  if (handtiles.fulu.size() * 3 + handtiles.lipai.size() != 14) {
    throw std::invalid_argument("winner hand snapshot does not form 14 tiles");
  }

  if (handtiles.IsZimo()) {
    handtiles.LastLipai().SetZimo();
  } else {
    handtiles.LastLipai().SetChonghu();
  }
  handtiles.SortLipaiWithoutLastOne();
  return handtiles;
}

proto::WinningContext BuildWinningContext(const proto::CandidateAction& candidate_action,
                                          const mahjong::Fan& fan) {
  proto::WinningContext winning_context;
  winning_context.set_win_type(ExtractWinType(candidate_action));
  winning_context.set_winning_tile(ExtractWinningTile(candidate_action));
  winning_context.set_completes_seven_pairs(
      HasFan(fan, mahjong::FAN_QIDUI) || HasFan(fan, mahjong::FAN_LIANQIDUI));
  winning_context.set_completes_thirteen_orphans(
      HasFan(fan, mahjong::FAN_SHISANYAO));
  winning_context.set_completes_standard_hand(
      !winning_context.completes_seven_pairs() &&
      !winning_context.completes_thirteen_orphans() &&
      !HasFan(fan, mahjong::FAN_QIXINGBUKAO) &&
      !HasFan(fan, mahjong::FAN_QUANBUKAO));
  winning_context.set_tentative_total_fan(fan.tot_fan_res);
  PopulateFanDetails(fan, winning_context.mutable_tentative_fan_details());
  return winning_context;
}

}  // namespace

proto::ActionKind GbMahjongAdapter::DeduceActionKind(
    const proto::CandidateAction& candidate_action) {
  switch (candidate_action.action_case()) {
    case proto::CandidateAction::kDiscard:
      return proto::ACTION_KIND_DISCARD;
    case proto::CandidateAction::kClaim:
      switch (candidate_action.claim().claim_kind()) {
        case proto::CLAIM_KIND_CHI:
          return proto::ACTION_KIND_CHI;
        case proto::CLAIM_KIND_PENG:
          return proto::ACTION_KIND_PENG;
        case proto::CLAIM_KIND_EXPOSED_KONG:
          return proto::ACTION_KIND_EXPOSED_KONG;
        case proto::CLAIM_KIND_DISCARD_WIN:
        case proto::CLAIM_KIND_ROB_KONG_WIN:
          return proto::ACTION_KIND_DECLARE_WIN;
        case proto::CLAIM_KIND_UNSPECIFIED:
        default:
          return proto::ACTION_KIND_UNSPECIFIED;
      }
    case proto::CandidateAction::kDeclareWin:
      return proto::ACTION_KIND_DECLARE_WIN;
    case proto::CandidateAction::kPass:
      return proto::ACTION_KIND_PASS;
    case proto::CandidateAction::kKong:
      return candidate_action.kong().kong_kind();
    case proto::CandidateAction::kReplaceFlower:
      return proto::ACTION_KIND_REPLACE_FLOWER;
    case proto::CandidateAction::ACTION_NOT_SET:
    default:
      return proto::ACTION_KIND_UNSPECIFIED;
  }
}

proto::MeldKind GbMahjongAdapter::DeduceMeldKind(
    const proto::ClaimAction& claim_action) {
  switch (claim_action.claim_kind()) {
    case proto::CLAIM_KIND_CHI:
      return proto::MELD_KIND_CHI;
    case proto::CLAIM_KIND_PENG:
      return proto::MELD_KIND_PENG;
    case proto::CLAIM_KIND_EXPOSED_KONG:
      return proto::MELD_KIND_EXPOSED_KONG;
    case proto::CLAIM_KIND_DISCARD_WIN:
    case proto::CLAIM_KIND_ROB_KONG_WIN:
    case proto::CLAIM_KIND_UNSPECIFIED:
    default:
      return proto::MELD_KIND_UNSPECIFIED;
  }
}

GbMahjongAdapter::ValidateOutcome GbMahjongAdapter::ValidateAction(
    const proto::ValidateActionRequest& request) const {
  ValidateOutcome outcome;

  try {
    if (IsBlank(request.room_id()) || IsBlank(request.match_id()) ||
        IsBlank(request.round_id())) {
      throw std::invalid_argument(
          "room_id, match_id, and round_id must all be present");
    }

    if (!request.has_rule_config() || !request.has_round_context() ||
        !request.has_candidate_action()) {
      throw std::invalid_argument(
          "rule_config, round_context, and candidate_action are required");
    }

    if (request.actor_seat() == proto::SEAT_UNSPECIFIED) {
      throw std::invalid_argument("actor_seat must not be unspecified");
    }

    const proto::CandidateAction& candidate_action = request.candidate_action();
    if (candidate_action.action_case() ==
        proto::CandidateAction::ACTION_NOT_SET) {
      throw std::invalid_argument("candidate_action.action must be set");
    }

    outcome.legal = true;
    outcome.derived_action_type = DeduceActionKind(candidate_action);

    if (IsWinningAction(candidate_action)) {
      mahjong::Handtiles handtiles = BuildWinningHandtilesForValidation(request);
      mahjong::Fan fan;
      const bool legal = fan.JudgeHu(handtiles);
      outcome.legal = legal;
      outcome.derived_action_type = proto::ACTION_KIND_DECLARE_WIN;

      if (!legal) {
        outcome.reject_code = proto::VALIDATION_REJECT_CODE_HAND_NOT_COMPLETE;
        outcome.explanation =
            "GB-Mahjong judged the candidate winning hand as incomplete";
        return outcome;
      }

      fan.CountFan(handtiles);
      outcome.winning_context = BuildWinningContext(candidate_action, fan);
      outcome.explanation =
          "winning action validated by GB-Mahjong using real hand evaluation";
      return outcome;
    }

    outcome.explanation =
        "GB-Mahjong does not expose a full action-window legality API; non-winning actions remain state-machine validated";

    if (candidate_action.action_case() == proto::CandidateAction::kClaim &&
        !IsWinningClaimKind(candidate_action.claim().claim_kind())) {
      proto::MeldResult meld_result;
      proto::Meld* meld = meld_result.mutable_meld();
      meld->set_kind(DeduceMeldKind(candidate_action.claim()));
      meld->set_from_seat(candidate_action.claim().source_seat());
      meld->set_claimed_tile(candidate_action.claim().target_tile());
      meld->set_source_event_seq(candidate_action.claim().source_event_seq());

      for (int tile : candidate_action.claim().consume_tiles()) {
        const auto typed_tile = static_cast<proto::Tile>(tile);
        meld->add_tiles(typed_tile);
        meld_result.add_consumed_tiles(typed_tile);
      }

      meld->add_tiles(candidate_action.claim().target_tile());
      meld_result.set_target_tile(candidate_action.claim().target_tile());
      outcome.meld_result = std::move(meld_result);
    }
  } catch (const std::exception& error) {
    outcome.legal = false;
    outcome.reject_code = proto::VALIDATION_REJECT_CODE_INVALID_CONTEXT;
    outcome.explanation = error.what();
  }

  return outcome;
}

GbMahjongAdapter::ScoreOutcome GbMahjongAdapter::CalculateScore(
    const proto::CalculateScoreRequest& request) const {
  if (!request.has_round_context() || !request.has_rule_config()) {
    throw std::invalid_argument(
        "CalculateScoreRequest requires round_context and rule_config");
  }

  if (request.winner_seat() == proto::SEAT_UNSPECIFIED) {
    throw std::invalid_argument("winner_seat must not be unspecified");
  }

  const proto::HandSnapshot& winner_hand = FindWinnerHand(request);
  mahjong::Handtiles handtiles =
      BuildWinningHandtilesForScoring(request, winner_hand);

  mahjong::Fan fan;
  if (!fan.JudgeHu(handtiles)) {
    throw std::invalid_argument(
        "winner hand snapshot is not a valid winning hand for GB-Mahjong");
  }

  fan.CountFan(handtiles);

  ScoreOutcome outcome;
  outcome.total_fan = static_cast<uint32_t>(fan.tot_fan_res);
  outcome.settlement_flags.reserve(request.settlement_flags_size());
  PopulateFanDetails(fan, &outcome.fan_details);

  for (int flag : request.settlement_flags()) {
    outcome.settlement_flags.push_back(
        static_cast<proto::SettlementFlag>(flag));
  }

  const int64_t base_points =
      std::max<int64_t>(1, request.rule_config().base_points());
  const int64_t unit_delta =
      base_points * static_cast<int64_t>(fan.tot_fan_res);

  for (const auto& hand : request.hands()) {
    proto::SeatScoreDelta score_delta;
    score_delta.set_seat(hand.seat());

    int64_t delta = 0;
    if (hand.seat() == request.winner_seat()) {
      if (request.win_type() == proto::WIN_TYPE_SELF_DRAW ||
          request.win_type() == proto::WIN_TYPE_KONG_DRAW) {
        delta = unit_delta * (static_cast<int64_t>(request.hands_size()) - 1);
      } else {
        delta = unit_delta;
      }
    } else if (request.win_type() == proto::WIN_TYPE_SELF_DRAW ||
               request.win_type() == proto::WIN_TYPE_KONG_DRAW) {
      delta = -unit_delta;
    } else if (hand.seat() == request.discarder_seat()) {
      delta = -unit_delta;
    }

    score_delta.set_delta(delta);
    score_delta.set_final_total(hand.current_score() + delta);
    outcome.score_delta_by_seat.push_back(std::move(score_delta));
  }

  return outcome;
}

}  // namespace gb_mahjong::engine_server
