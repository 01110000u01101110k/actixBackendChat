use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use std::time::{Duration, Instant};

use actix::*;
use actix_files as fs;
use actix_web::{web, App, Error, HttpRequest, HttpResponse, HttpServer, Responder};
use actix_web_actors::ws;

mod server;

/// Как часто отправляются пинги сердцебиения
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5);
/// Через какое время отсутствие ответа клиента приводит к тайм-ауту
const CLIENT_TIMEOUT: Duration = Duration::from_secs(10);

/// Точка входа для нашего маршрута websocket
async fn chat_route(
    req: HttpRequest,
    stream: web::Payload,
    srv: web::Data<Addr<server::ChatServer>>,
) -> Result<HttpResponse, Error> {
    ws::start(
        WsChatSession {
            id: 0,
            hb: Instant::now(),
            room: "Main".to_owned(),
            name: None,
            addr: srv.get_ref().clone(),
        },
        &req,
        stream,
    )
}

/// Отображает и влияет на состояние
async fn get_count(count: web::Data<Arc<AtomicUsize>>) -> impl Responder {
    let current_count = count.fetch_add(1, Ordering::SeqCst);
    format!("Visitors: {}", current_count)
}

struct WsChatSession {
    /// уникальный идентификатор сессии
    id: usize,
    /// Клиент должен отправлять ping не реже одного раза в 10 секунд (CLIENT_TIMEOUT), иначе мы разрываем соединение.
    hb: Instant,
    /// объединённая комната
    room: String,
    /// имя
    name: Option<String>,
    /// Сервер чата
    addr: Addr<server::ChatServer>,
}

impl Actor for WsChatSession {
    type Context = ws::WebsocketContext<Self>;

    /// Метод вызывается при запуске актера.
    /// Мы регистрируем сессию ws на ChatServer
    fn started(&mut self, ctx: &mut Self::Context) {
        // мы запустим процесс сердцебиения при старте сессии.
        self.hb(ctx);

        // зарегистрировать себя на сервере чата. `AsyncContext::wait` регистрирует будущее в контексте, но контекст ждет, пока это будущее разрешится, прежде чем обрабатывать любые другие события.
        // HttpContext::state() является экземпляром WsChatSessionState, состояние разделяется между всеми маршрутами внутри приложения
        let addr = ctx.address();
        self.addr
            .send(server::Connect {
                addr: addr.recipient(),
            })
            .into_actor(self)
            .then(|res, act, ctx| {
                match res {
                    Ok(res) => act.id = res,
                    // что-то не так с сервером чата
                    _ => ctx.stop(),
                }
                fut::ready(())
            })
            .wait(ctx);
    }

    fn stopping(&mut self, _: &mut Self::Context) -> Running {
        // уведомлять сервер чата
        self.addr.do_send(server::Disconnect { id: self.id });
        Running::Stop
    }
}

/// Обработка сообщений от сервера чата, мы просто отправляем их на одноранговый вебсокет
impl Handler<server::Message> for WsChatSession {
    type Result = ();

    fn handle(&mut self, msg: server::Message, ctx: &mut Self::Context) {
        ctx.text(msg.0);
    }
}

/// Обработчик сообщений WebSocket
impl StreamHandler<Result<ws::Message, ws::ProtocolError>> for WsChatSession {
    fn handle(
        &mut self,
        msg: Result<ws::Message, ws::ProtocolError>,
        ctx: &mut Self::Context,
    ) {
        let msg = match msg {
            Err(_) => {
                ctx.stop();
                return;
            }
            Ok(msg) => msg,
        };

        println!("WEBSOCKET MESSAGE: {:?}", msg);
        match msg {
            ws::Message::Ping(msg) => {
                self.hb = Instant::now();
                ctx.pong(&msg);
            }
            ws::Message::Pong(_) => {
                self.hb = Instant::now();
            }
            ws::Message::Text(text) => {
                let m = text.trim();
                // мы проверяем сообщения типа /sss
                if m.starts_with('/') {
                    let v: Vec<&str> = m.splitn(2, ' ').collect();
                    match v[0] {
                        "/list" => {
                            // Отправьте сообщение ListRooms на сервер чата и дождитесь ответа
                            println!("List rooms");
                            self.addr
                                .send(server::ListRooms)
                                .into_actor(self)
                                .then(|res, _, ctx| {
                                    match res {
                                        Ok(rooms) => {
                                            for room in rooms {
                                                ctx.text(room);
                                            }
                                        }
                                        _ => println!("Something is wrong"),
                                    }
                                    fut::ready(())
                                })
                                .wait(ctx)
                            // .wait(ctx) приостанавливает все события в контексте, поэтому актор не будет получать новые сообщения, пока не получит список комнат обратно
                        }
                        "/join" => {
                            if v.len() == 2 {
                                self.room = v[1].to_owned();
                                self.addr.do_send(server::Join {
                                    id: self.id,
                                    name: self.room.clone(),
                                });

                                ctx.text("joined");
                            } else {
                                ctx.text("!!! room name is required");
                            }
                        }
                        "/name" => {
                            if v.len() == 2 {
                                self.name = Some(v[1].to_owned());
                            } else {
                                ctx.text("!!! name is required");
                            }
                        }
                        _ => ctx.text(format!("!!! unknown command: {:?}", m)),
                    }
                } else {
                    let msg = if let Some(ref name) = self.name {
                        format!("{}: {}", name, m)
                    } else {
                        m.to_owned()
                    };
                    // отправить сообщение на сервер чата
                    self.addr.do_send(server::ClientMessage {
                        id: self.id,
                        msg,
                        room: self.room.clone(),
                    })
                }
            }
            ws::Message::Binary(_) => println!("Unexpected binary"),
            ws::Message::Close(reason) => {
                ctx.close(reason);
                ctx.stop();
            }
            ws::Message::Continuation(_) => {
                ctx.stop();
            }
            ws::Message::Nop => (),
        }
    }
}

impl WsChatSession {
    /// вспомогательный метод, который отправляет ping клиенту каждую секунду.
    /// также этот метод проверяет сердцебиение клиента
    fn hb(&self, ctx: &mut ws::WebsocketContext<Self>) {
        ctx.run_interval(HEARTBEAT_INTERVAL, |act, ctx| {
            // проверять сердцебиение клиента
            if Instant::now().duration_since(act.hb) > CLIENT_TIMEOUT {
                // сердцебиение прервано
                println!("Websocket Client heartbeat failed, disconnecting!");

                // уведомлять сервер чата
                act.addr.do_send(server::Disconnect { id: act.id });

                // остановить актёра
                ctx.stop();

                // не пытайтесь посылать ping
                return;
            }

            ctx.ping(b"");
        });
    }
}

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    env_logger::init();

    // Состояние приложения
    // Мы ведем подсчет количества посетителей
    let app_state = Arc::new(AtomicUsize::new(0));

    // Запуск актера сервера чата
    let server = server::ChatServer::new(app_state.clone()).start();

    // Создание Http-сервера с поддержкой вебсокета
    HttpServer::new(move || {
        App::new()
            .data(app_state.clone())
            .data(server.clone())
            .route("/count/", web::get().to(get_count))
            // websocket
            .service(web::resource("/ws/").to(chat_route))
    })
    .bind("127.0.0.1:8081")?
    .run()
    .await
}