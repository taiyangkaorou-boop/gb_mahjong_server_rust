#pragma once

#include <memory>

#include <grpcpp/grpcpp.h>

#include "gb_mahjong_adapter.h"
#include "internal_engine.grpc.pb.h"

namespace gb_mahjong::engine_server {

// gRPC service 层尽量保持轻薄：
// 它只负责承接 RPC、调用适配层、回填 protobuf 响应。
class RuleEngineServiceImpl final
    : public gb_mahjong::engine::v1::RuleEngineService::Service {
 public:
  explicit RuleEngineServiceImpl(std::shared_ptr<GbMahjongAdapter> adapter);

  ::grpc::Status ValidateAction(
      ::grpc::ServerContext* context,
      const gb_mahjong::engine::v1::ValidateActionRequest* request,
      gb_mahjong::engine::v1::ValidateActionResponse* response) override;

  ::grpc::Status CalculateScore(
      ::grpc::ServerContext* context,
      const gb_mahjong::engine::v1::CalculateScoreRequest* request,
      gb_mahjong::engine::v1::CalculateScoreResponse* response) override;

 private:
  // service 本身不持有任何房间状态，只持有一个可复用的规则适配器实例。
  std::shared_ptr<GbMahjongAdapter> adapter_;
};

}  // namespace gb_mahjong::engine_server
