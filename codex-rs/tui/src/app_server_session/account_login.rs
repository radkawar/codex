use super::AppServerSession;
use crate::app_event::AccountLoginMethod;
use codex_app_server_protocol::CancelLoginAccountParams;
use codex_app_server_protocol::CancelLoginAccountResponse;
use codex_app_server_protocol::CancelLoginAccountStatus;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::LoginAccountParams;
use codex_app_server_protocol::LoginAccountResponse;
use color_eyre::eyre::Result;
use color_eyre::eyre::WrapErr;

impl AppServerSession {
    pub(crate) async fn start_account_session_login(
        &mut self,
        method: AccountLoginMethod,
    ) -> Result<LoginAccountResponse> {
        let request_id = self.next_request_id();
        let params = match method {
            AccountLoginMethod::Browser => LoginAccountParams::Chatgpt {
                app_brand: None,
                codex_streamlined_login: false,
                use_hosted_login_success_page: false,
            },
            AccountLoginMethod::DeviceCode => LoginAccountParams::ChatgptDeviceCode,
        };
        self.client
            .request_typed(ClientRequest::AccountSessionsLogin { request_id, params })
            .await
            .wrap_err("accountSession/login/start failed in TUI")
    }

    pub(crate) async fn cancel_account_session_login(
        &mut self,
        login_id: String,
    ) -> Result<CancelLoginAccountStatus> {
        let request_id = self.next_request_id();
        let response: CancelLoginAccountResponse = self
            .client
            .request_typed(ClientRequest::CancelLoginAccount {
                request_id,
                params: CancelLoginAccountParams { login_id },
            })
            .await
            .wrap_err("account/login/cancel failed in TUI")?;
        Ok(response.status)
    }
}
