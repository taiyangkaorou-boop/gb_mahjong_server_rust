// RoomManager 只负责房间任务的生命周期和命令投递。
// 这里不持有任何单个房间的可变状态，避免把并发问题带回网关层。

impl RoomManager {
    // 这一段只负责 RoomManager 对外暴露的消息入口：
    // 它把网关请求转成 RoomCommand，并负责首次创建 room task。
    pub fn with_rule_engine(rule_engine: RuleEngineHandle) -> Self {
        Self::with_rule_engine_and_event_writer(rule_engine, MatchEventWriter::noop())
    }

    pub fn with_rule_engine_and_event_writer(
        rule_engine: RuleEngineHandle,
        match_event_writer: MatchEventWriter,
    ) -> Self {
        Self {
            rooms: Arc::new(RwLock::new(HashMap::new())),
            rule_engine,
            wall_factory: Arc::new(build_shuffled_wall),
            match_event_writer,
        }
    }

    #[allow(dead_code)]
    pub fn with_rule_engine_and_wall_factory(
        rule_engine: RuleEngineHandle,
        wall_factory: WallFactory,
    ) -> Self {
        Self {
            rooms: Arc::new(RwLock::new(HashMap::new())),
            rule_engine,
            wall_factory,
            match_event_writer: MatchEventWriter::noop(),
        }
    }

    pub async fn dispatch_join(
        &self,
        request_id: String,
        connection: ConnectionHandle,
        request: JoinRoomRequest,
        authorized_join: AuthorizedJoin,
    ) {
        let room_id = request.room_id.clone();
        let sender = self.get_or_create_room_sender(room_id.clone()).await;

        self.send_command(
            sender,
            RoomCommand::Join {
                request_id,
                connection: connection.clone(),
                request,
                authorized_join,
            },
            Some((connection, room_id)),
        )
        .await;
    }

    pub async fn dispatch_ready(
        &self,
        room_id: String,
        request_id: String,
        connection: ConnectionHandle,
        request: ReadyRequest,
    ) {
        let Some(sender) = self.get_room_sender(&room_id).await else {
            connection.send_frame(build_action_rejected(
                0,
                request_id,
                RejectCode::RoomNotFound,
                "room task not found for ReadyRequest",
                0,
                0,
            ));
            return;
        };

        self.send_command(
            sender,
            RoomCommand::Ready {
                request_id,
                connection_id: connection.connection_id().to_owned(),
                request,
            },
            Some((connection, room_id)),
        )
        .await;
    }

    pub async fn dispatch_resume(
        &self,
        request_id: String,
        connection: ConnectionHandle,
        request: ResumeSessionRequest,
    ) {
        let room_id = request.room_id.clone();
        let Some(sender) = self.get_room_sender(&room_id).await else {
            connection.send_frame(build_action_rejected(
                0,
                request_id,
                RejectCode::RoomNotFound,
                "room task not found for ResumeSessionRequest",
                0,
                0,
            ));
            return;
        };

        self.send_command(
            sender,
            RoomCommand::ResumeSession {
                request_id,
                connection: connection.clone(),
                request,
            },
            Some((connection, room_id)),
        )
        .await;
    }

    pub async fn dispatch_player_action(
        &self,
        request_id: String,
        connection: ConnectionHandle,
        request: PlayerActionRequest,
    ) {
        let room_id = request.room_id.clone();
        let Some(sender) = self.get_room_sender(&room_id).await else {
            connection.send_frame(build_action_rejected(
                0,
                request_id,
                RejectCode::RoomNotFound,
                "room task not found for PlayerActionRequest",
                request.expected_event_seq,
                request.action_window_id,
            ));
            return;
        };

        self.send_command(
            sender,
            RoomCommand::PlayerAction {
                request_id,
                connection_id: connection.connection_id().to_owned(),
                request,
            },
            Some((connection, room_id)),
        )
        .await;
    }

    pub async fn disconnect(&self, room_id: &str, connection_id: &str) {
        let Some(sender) = self.get_room_sender(room_id).await else {
            return;
        };

        if let Err(error) = sender
            .send(RoomCommand::Disconnect {
                connection_id: connection_id.to_owned(),
            })
            .await
        {
            debug!(
                %room_id,
                %connection_id,
                ?error,
                "room task already stopped before disconnect command"
            );
        }
    }

    pub async fn dispatch_remove_player(&self, room_id: String, user_id: String) {
        // 这个入口只处理“大厅阶段的成员变更”对 room task 的同步，
        // 已开局后的真实对局不走这里踢人。
        let Some(sender) = self.get_room_sender(&room_id).await else {
            return;
        };

        if let Err(error) = sender.send(RoomCommand::RemovePlayer { user_id }).await {
            debug!(%room_id, ?error, "room task already stopped before remove player command");
        }
    }

    async fn send_command(
        &self,
        sender: mpsc::Sender<RoomCommand>,
        command: RoomCommand,
        fallback: Option<(ConnectionHandle, String)>,
    ) {
        if let Err(error) = sender.send(command).await {
            if let Some((connection, room_id)) = fallback {
                connection.send_frame(build_action_rejected(
                    0,
                    String::new(),
                    RejectCode::InternalError,
                    format!("failed to enqueue command for room {room_id}: {error}"),
                    0,
                    0,
                ));
            }
        }
    }

    async fn get_room_sender(&self, room_id: &str) -> Option<mpsc::Sender<RoomCommand>> {
        let rooms = self.rooms.read().await;
        rooms.get(room_id).map(|handle| handle.command_tx.clone())
    }

    async fn get_or_create_room_sender(&self, room_id: String) -> mpsc::Sender<RoomCommand> {
        if let Some(sender) = self.get_room_sender(&room_id).await {
            return sender;
        }

        let mut rooms = self.rooms.write().await;

        if let Some(handle) = rooms.get(&room_id) {
            return handle.command_tx.clone();
        }

        let (command_tx, command_rx) = mpsc::channel(ROOM_COMMAND_BUFFER);
        let room_state = RoomState::new(
            room_id.clone(),
            self.rule_engine.clone(),
            self.wall_factory.clone(),
            command_tx.clone(),
            self.match_event_writer.clone(),
        );

        info!(%room_id, "spawning room task");
        tokio::spawn(run_room_task(room_state, command_rx));

        rooms.insert(
            room_id,
            RoomHandle {
                command_tx: command_tx.clone(),
            },
        );

        command_tx
    }
}
