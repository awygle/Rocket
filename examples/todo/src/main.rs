#[macro_use] extern crate rocket;
#[macro_use] extern crate rocket_sync_db_pools;
#[macro_use] extern crate diesel;

#[cfg(test)]
mod tests;
mod task;

use rocket::{Rocket, Build};
use rocket::fairing::AdHoc;
use rocket::request::FlashMessage;
use rocket::response::{Flash, Redirect};
use rocket::serde::Serialize;
use rocket::form::Form;
use rocket::fs::{FileServer, relative};

use rocket_dyn_templates::Template;

use crate::task::{Task, Todo};


////

use rocket_sync_db_pools::{Poolable, PoolResult, Config};
use diesel::r2d2::ManageConnection;
use core::ops::{Deref, DerefMut};
use core::time::Duration;

pub struct SqlcipherConnection(diesel::SqliteConnection);
pub struct SqlcipherConnectionManager(diesel::r2d2::ConnectionManager<diesel::SqliteConnection>);

impl Deref for SqlcipherConnection {
    type Target = diesel::SqliteConnection;
    
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl DerefMut for SqlcipherConnection {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl ManageConnection for SqlcipherConnectionManager {
    type Connection = SqlcipherConnection;
    type Error = <diesel::r2d2::ConnectionManager<diesel::SqliteConnection> as ManageConnection>::Error;
    
    fn connect(&self) -> Result <Self::Connection, Self::Error> {
        self.0.connect().map(|x| SqlcipherConnection(x))
    }
    
    fn is_valid(&self, conn: &mut Self::Connection) -> Result<(), Self::Error> {
        self.0.is_valid(&mut conn.0)
    }
    
    fn has_broken(&self, conn: &mut Self::Connection) -> bool {
        self.0.has_broken(&mut conn.0)
    }
}

impl Poolable for SqlcipherConnection {
    type Manager = SqlcipherConnectionManager;
    type Error = <diesel::SqliteConnection as Poolable>::Error;

    fn pool(db_name: &str, rocket: &Rocket<Build>) -> PoolResult<Self> {
        use diesel::{SqliteConnection, connection::SimpleConnection};
        use diesel::r2d2::{CustomizeConnection, ConnectionManager, Error, Pool};

        #[derive(Debug)]
        struct Customizer;

        impl CustomizeConnection<SqlcipherConnection, Error> for Customizer {
            fn on_acquire(&self, conn: &mut SqlcipherConnection) -> Result<(), Error> {
                conn.0.batch_execute("\
                    PRAGMA key = apples;\
                    PRAGMA journal_mode = WAL;\
                    PRAGMA busy_timeout = 1000;\
                    PRAGMA foreign_keys = ON;\
                ").map_err(Error::QueryError)?;

                Ok(())
            }
        }

        let config = Config::from(db_name, rocket)?;
        let manager = SqlcipherConnectionManager(ConnectionManager::new(&config.url));
        let pool = Pool::builder()
            .connection_customizer(Box::new(Customizer))
            .max_size(config.pool_size)
            .connection_timeout(Duration::from_secs(config.timeout as u64))
            .build(manager)?;

        Ok(pool)
    }
}


////

#[database("sqlite_database")]
pub struct DbConn(SqlcipherConnection);

#[derive(Debug, Serialize)]
#[serde(crate = "rocket::serde")]
struct Context {
    flash: Option<(String, String)>,
    tasks: Vec<Task>
}

impl Context {
    pub async fn err<M: std::fmt::Display>(conn: &DbConn, msg: M) -> Context {
        Context {
            flash: Some(("error".into(), msg.to_string())),
            tasks: Task::all(conn).await.unwrap_or_default()
        }
    }

    pub async fn raw(conn: &DbConn, flash: Option<(String, String)>) -> Context {
        match Task::all(conn).await {
            Ok(tasks) => Context { flash, tasks },
            Err(e) => {
                error_!("DB Task::all() error: {}", e);
                Context {
                    flash: Some(("error".into(), "Fail to access database.".into())),
                    tasks: vec![]
                }
            }
        }
    }
}

#[post("/", data = "<todo_form>")]
async fn new(todo_form: Form<Todo>, conn: DbConn) -> Flash<Redirect> {
    let todo = todo_form.into_inner();
    if todo.description.is_empty() {
        Flash::error(Redirect::to("/"), "Description cannot be empty.")
    } else if let Err(e) = Task::insert(todo, &conn).await {
        error_!("DB insertion error: {}", e);
        Flash::error(Redirect::to("/"), "Todo could not be inserted due an internal error.")
    } else {
        Flash::success(Redirect::to("/"), "Todo successfully added.")
    }
}

#[put("/<id>")]
async fn toggle(id: i32, conn: DbConn) -> Result<Redirect, Template> {
    match Task::toggle_with_id(id, &conn).await {
        Ok(_) => Ok(Redirect::to("/")),
        Err(e) => {
            error_!("DB toggle({}) error: {}", id, e);
            Err(Template::render("index", Context::err(&conn, "Failed to toggle task.").await))
        }
    }
}

#[delete("/<id>")]
async fn delete(id: i32, conn: DbConn) -> Result<Flash<Redirect>, Template> {
    match Task::delete_with_id(id, &conn).await {
        Ok(_) => Ok(Flash::success(Redirect::to("/"), "Todo was deleted.")),
        Err(e) => {
            error_!("DB deletion({}) error: {}", id, e);
            Err(Template::render("index", Context::err(&conn, "Failed to delete task.").await))
        }
    }
}

#[get("/")]
async fn index(flash: Option<FlashMessage<'_>>, conn: DbConn) -> Template {
    let flash = flash.map(FlashMessage::into_inner);
    Template::render("index", Context::raw(&conn, flash).await)
}

async fn run_migrations(rocket: Rocket<Build>) -> Rocket<Build> {
    use diesel_migrations::{embed_migrations, EmbeddedMigrations, MigrationHarness};

    const MIGRATIONS: EmbeddedMigrations = embed_migrations!("migrations");

    DbConn::get_one(&rocket).await
        .expect("database connection")
        .run(|conn| { conn.run_pending_migrations(MIGRATIONS).expect("diesel migrations"); })
        .await;

    rocket
}

#[launch]
fn rocket() -> _ {
    rocket::build()
        .attach(DbConn::fairing())
        .attach(Template::fairing())
        .attach(AdHoc::on_ignite("Run Migrations", run_migrations))
        .mount("/", FileServer::from(relative!("static")))
        .mount("/", routes![index])
        .mount("/todo", routes![new, toggle, delete])
}
