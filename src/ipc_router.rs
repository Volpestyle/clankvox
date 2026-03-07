use crate::app_state::AppState;
use crate::ipc::InMsg;
use crate::ipc_protocol::RoutedInMsg;

impl AppState {
    pub(crate) async fn route_ipc_message(&mut self, msg: InMsg) -> bool {
        match RoutedInMsg::try_from(msg).expect("all inbound IPC messages should route") {
            RoutedInMsg::Connection(command) => {
                self.handle_connection_command(command).await;
                false
            }
            RoutedInMsg::Capture(command) => {
                self.handle_capture_command(command);
                false
            }
            RoutedInMsg::Playback(command) => self.handle_playback_command(command),
        }
    }
}
