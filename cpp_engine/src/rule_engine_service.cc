#include "rule_engine_service.h"

#include <iostream>
#include <stdexcept>
#include <utility>

namespace proto = gb_mahjong::engine::v1;

namespace gb_mahjong::engine_server {

RuleEngineServiceImpl::RuleEngineServiceImpl(
    std::shared_ptr<GbMahjongAdapter> adapter)
    : adapter_(std::move(adapter)) {}

::grpc::Status RuleEngineServiceImpl::ValidateAction(
    ::grpc::ServerContext* context, const proto::ValidateActionRequest* request,
    proto::ValidateActionResponse* response) {
  (void)context;

  // service 层尽量保持“薄”，把 protobuf <-> 领域逻辑的桥接交给 adapter。
  const GbMahjongAdapter::ValidateOutcome outcome =
      adapter_->ValidateAction(*request);

  response->set_is_legal(outcome.legal);
  response->set_reject_code(outcome.reject_code);
  response->set_derived_action_type(outcome.derived_action_type);
  response->set_explanation(outcome.explanation);

  if (outcome.meld_result.has_value()) {
    *response->mutable_meld_result() = *outcome.meld_result;
  }

  if (outcome.winning_context.has_value()) {
    *response->mutable_winning_context() = *outcome.winning_context;
  }

  for (const auto& follow_up : outcome.follow_ups) {
    *response->add_suggested_follow_ups() = follow_up;
  }

  std::cout << "[ValidateAction] room_id=" << request->room_id()
            << " match_id=" << request->match_id()
            << " round_id=" << request->round_id()
            << " actor_seat=" << request->actor_seat()
            << " legal=" << outcome.legal << std::endl;

  return ::grpc::Status::OK;
}

::grpc::Status RuleEngineServiceImpl::CalculateScore(
    ::grpc::ServerContext* context, const proto::CalculateScoreRequest* request,
    proto::CalculateScoreResponse* response) {
  (void)context;

  // service 层不自行吞掉异常含义，而是尽量把输入错误/内部错误映射回 gRPC 状态码。
  // 算分阶段对输入完整性要求更高，因此这里把 adapter 抛出的参数错误映射成 INVALID_ARGUMENT。
  GbMahjongAdapter::ScoreOutcome outcome;
  try {
    outcome = adapter_->CalculateScore(*request);
  } catch (const std::invalid_argument& error) {
    return ::grpc::Status(::grpc::StatusCode::INVALID_ARGUMENT, error.what());
  } catch (const std::exception& error) {
    return ::grpc::Status(::grpc::StatusCode::INTERNAL, error.what());
  }

  response->set_total_fan(outcome.total_fan);

  for (const auto& fan_detail : outcome.fan_details) {
    *response->add_fan_details() = fan_detail;
  }

  for (const auto& seat_score_delta : outcome.score_delta_by_seat) {
    *response->add_score_delta_by_seat() = seat_score_delta;
  }

  for (const auto settlement_flag : outcome.settlement_flags) {
    response->add_settlement_flags(settlement_flag);
  }

  std::cout << "[CalculateScore] room_id=" << request->room_id()
            << " match_id=" << request->match_id()
            << " round_id=" << request->round_id()
            << " winner_seat=" << request->winner_seat()
            << " total_fan=" << outcome.total_fan << std::endl;

  return ::grpc::Status::OK;
}

}  // namespace gb_mahjong::engine_server
