//! Native client for Adapt.

use adapt::{
    client::Context,
    essence::models,
    http::{Http, endpoints},
    ws::{Client as Ws, ConnectOptions, FallibleEventHandler},
};
use iced::{
    Color, Length, Padding, Task,
    alignment::Horizontal,
    widget::{button, column, container, row, svg, text, text_input},
};
use log::{info, warn};
use secrecy::ExposeSecret;
use std::sync::Arc;
use tokio::sync::{Mutex, mpsc};

#[derive(Default, Debug, Copy, Clone, PartialEq, Eq)]
enum LoginBy {
    #[default]
    Email,
    Token,
}

#[derive(Default)]
struct LoginScreen {
    entered_email: String,
    entered_password: String,
    entered_token: String,

    login_by: LoginBy,
    login_error: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
enum LoginScreenMessage {
    EmailChanged(String),
    PasswordChanged(String),
    TokenChanged(String),
    SetLoginBy(LoginBy),
    SetLoginError(String),
    SubmitEmailPassword,
    SubmitToken,
}

impl LoginScreen {
    fn update(&mut self, message: LoginScreenMessage) -> Task<AppMessage> {
        match message {
            LoginScreenMessage::EmailChanged(email) => {
                self.entered_email = email;
            }
            LoginScreenMessage::PasswordChanged(password) => {
                self.entered_password = password;
            }
            LoginScreenMessage::TokenChanged(token) => {
                self.entered_token = token;
            }
            LoginScreenMessage::SetLoginBy(login_by) => {
                self.login_by = login_by;
            }
            LoginScreenMessage::SetLoginError(error) => {
                self.login_error = Some(error);
            }
            LoginScreenMessage::SubmitEmailPassword => {
                self.login_error = None;
                return Task::perform(
                    Http::login(
                        self.entered_email.clone(),
                        self.entered_password.clone(),
                        Default::default(),
                    ),
                    |result| match result {
                        Ok(http) => AppMessage::StartClient(http),
                        Err(e) => {
                            warn!("Login failed: {e:?}");

                            AppMessage::Login(LoginScreenMessage::SetLoginError(format!("{e:?}")))
                        }
                    },
                );
            }
            LoginScreenMessage::SubmitToken => {
                self.login_error = None;
                return Task::done(AppMessage::StartClient(Http::from_token(
                    self.entered_token.clone(),
                )));
            }
        }
        Task::none()
    }

    fn view(&self) -> iced::Element<'_, LoginScreenMessage> {
        let banner = svg("assets/banner-white-fg.svg").width(168);

        let banner = container(banner).padding(Padding::ZERO.bottom(12));

        let email_input = text_input("Email", &self.entered_email)
            .on_input(LoginScreenMessage::EmailChanged)
            .width(Length::Fill)
            .padding(10);

        let password_input = text_input("Password", &self.entered_password)
            .secure(true)
            .on_input(LoginScreenMessage::PasswordChanged)
            .width(Length::Fill)
            .padding(10);

        let token_input = text_input("Token", &self.entered_token)
            .on_input(LoginScreenMessage::TokenChanged)
            .width(Length::Fill)
            .padding(10);

        let login_button = button(
            text("Login")
                .align_x(Horizontal::Center)
                .width(Length::Fill),
        )
        .on_press(LoginScreenMessage::SubmitEmailPassword)
        .padding(10)
        .width(Length::Fill)
        .on_press(LoginScreenMessage::SubmitEmailPassword);

        let token_button = button("Login with Token")
            .on_press(LoginScreenMessage::SubmitToken)
            .padding(10)
            .width(Length::Fill)
            .on_press(LoginScreenMessage::SubmitToken);

        let mut content = match self.login_by {
            LoginBy::Email => column![banner, email_input, password_input, login_button,],
            LoginBy::Token => column![banner, token_input, token_button,],
        };

        if let Some(error) = self.login_error.as_ref() {
            content = content.push(text(error).color(Color::from_rgb(1.0, 0.0, 0.0)));
        }

        content = content.width(400).align_x(Horizontal::Center).spacing(10);

        container(content)
            .center_x(Length::Fill)
            .center_y(Length::Fill)
            .into()
    }
}

#[derive(Debug, Clone)]
enum AppMessage {
    Login(LoginScreenMessage),
    StartClient(Http),
    ReadyUp(adapt::essence::models::ClientUser),
    StopClient,
    Handle(Box<Self>),
}

impl From<LoginScreenMessage> for AppMessage {
    fn from(message: LoginScreenMessage) -> Self {
        AppMessage::Login(message)
    }
}

struct ClientState {
    ctx: Context,
    user: adapt::essence::models::ClientUser,
}

type MessageStream = Arc<Mutex<mpsc::UnboundedReceiver<AppMessage>>>;

enum App {
    Login(LoginScreen),
    Loading {
        rx: MessageStream,
        ctx: Context,
    },
    Main {
        rx: MessageStream,
        state: ClientState,
    },
}

// A proxy handler that just forwards events as AppMessage over a channel.
#[derive(Clone)]
struct AppHandler {
    tx: mpsc::UnboundedSender<AppMessage>,
}

impl FallibleEventHandler for AppHandler {
    type Error = adapt::Error;

    async fn on_error(&mut self, error: Self::Error) {
        warn!("error: {:?}", error);
    }

    async fn on_ready(&mut self, context: Context) -> Result<(), Self::Error> {
        // Do any synchronous HTTP you need (on the WS task)
        let user = context
            .http()
            .request(endpoints::GetAuthenticatedUser)
            .await?;
        info!("Logged in as: {}", user.username);

        // Tell the UI thread to transition
        let _ = self.tx.send(AppMessage::ReadyUp(user));
        Ok(())
    }
}

impl Default for App {
    fn default() -> Self {
        App::new_login()
    }
}

impl App {
    fn new_login() -> Self {
        App::Login(LoginScreen::default())
    }

    fn arm_recv(rx: &MessageStream) -> Task<AppMessage> {
        let rx = Arc::clone(rx);
        Task::perform(
            async move {
                let mut rx = rx.lock().await;
                rx.recv().await // Option<AppMessage>
            },
            |maybe| maybe.map_or(AppMessage::StopClient, |m| AppMessage::Handle(Box::new(m))),
        )
    }

    fn update(&mut self, message: AppMessage) -> Task<AppMessage> {
        match message {
            AppMessage::Login(login_message) => {
                if let App::Login(login_screen) = self {
                    return login_screen.update(login_message);
                }
            }
            AppMessage::StartClient(http) => {
                let http = Arc::new(http);

                let ws_options = ConnectOptions::new(http.token().expose_secret())
                    .device(models::Device::Desktop);

                let (tx, rx_raw) = mpsc::unbounded_channel::<AppMessage>();
                let rx = Arc::new(Mutex::new(rx_raw));
                let ws_handler = AppHandler { tx: tx.clone() };

                let ws = Arc::new(Ws::new(ws_options, ws_handler));
                let ctx = Context::from_http(http.clone());

                *self = App::Loading {
                    rx: Arc::clone(&rx),
                    ctx: ctx.clone(),
                };

                let recv_task = App::arm_recv(&rx); // <-- no borrow of `self`
                let ws_task = Task::perform(
                    async move {
                        // `start` can now borrow from `&*ws_task` for the lifetime of this future,
                        // and the future is `'static` because it owns `ws_task` and `ctx_task`.
                        ws.start(ctx).await
                    },
                    |_| {
                        info!("Client stopped");
                        AppMessage::StopClient
                    },
                );

                return Task::batch([recv_task, ws_task]);
            }
            AppMessage::ReadyUp(user) => {
                if let App::Loading { rx, ctx } = std::mem::take(self) {
                    *self = App::Main {
                        rx,
                        state: ClientState { ctx, user },
                    };
                }
            }
            AppMessage::StopClient => {
                info!("Exiting...");
                std::process::exit(0);
            }
            AppMessage::Handle(message) => {
                if let App::Main { rx, .. } | App::Loading { rx, .. } = self {
                    let rx = Arc::clone(rx);
                    let task = self.update(*message);
                    return Task::batch([task, App::arm_recv(&rx)]);
                }
            }
        }
        Task::none()
    }

    fn view(&self) -> iced::Element<'_, AppMessage> {
        match self {
            App::Login(login_screen) => login_screen.view().map(AppMessage::Login),
            App::Loading { .. } => container(text("Loading..."))
                .center_x(Length::Fill)
                .center_y(Length::Fill)
                .into(),
            App::Main { state, .. } => {
                let user_info = format!("Logged in as: {}", state.user.username);
                iced::widget::text(user_info).into()
            }
        }
    }
}

fn main() -> iced::Result {
    iced::run("Adapt", App::update, App::view)
}
