use std::{collections::HashMap, sync::Arc, time::{Duration, Instant}};
use axum::{extract::{Query, State}, http::{header, HeaderMap, StatusCode}, response::{IntoResponse, Redirect}, routing::{get, post}, Json, Router};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

#[derive(Clone)]struct AppState{client:reqwest::Client,client_id:String,client_secret:String,redirect_uri:String,tickets:Arc<Mutex<HashMap<String,(Instant,serde_json::Value)>>>}
#[derive(Deserialize)]struct Start{state:String}
#[derive(Deserialize)]struct Callback{code:String,state:String}
#[derive(Deserialize)]struct Redeem{ticket:String}
#[derive(Serialize)]struct ErrorBody{error:String}

async fn start(State(s):State<AppState>,Query(q):Query<Start>)->Redirect{let target=format!("https://bgm.tv/oauth/authorize?client_id={}&response_type=code&redirect_uri={}&state={}",urlencoding::encode(&s.client_id),urlencoding::encode(&s.redirect_uri),urlencoding::encode(&q.state));Redirect::temporary(&target)}
async fn callback(State(s):State<AppState>,Query(q):Query<Callback>)->impl IntoResponse{
 let body=serde_json::json!({"grant_type":"authorization_code","client_id":s.client_id,"client_secret":s.client_secret,"code":q.code,"redirect_uri":s.redirect_uri,"state":q.state});
 match s.client.post("https://bgm.tv/oauth/access_token").json(&body).send().await{Ok(r)=>match r.error_for_status(){Ok(r)=>match r.json::<serde_json::Value>().await{Ok(token)=>{let ticket=uuid::Uuid::new_v4().to_string();s.tickets.lock().await.insert(ticket.clone(),(Instant::now()+Duration::from_secs(60),token));Redirect::temporary(&format!("mizuki://oauth/callback?ticket={}&state={}",ticket,urlencoding::encode(&q.state))).into_response()},Err(e)=>(StatusCode::BAD_GATEWAY,Json(ErrorBody{error:e.to_string()})).into_response()},Err(e)=>(StatusCode::BAD_GATEWAY,Json(ErrorBody{error:e.to_string()})).into_response()},Err(e)=>(StatusCode::BAD_GATEWAY,Json(ErrorBody{error:e.to_string()})).into_response()}
}
async fn redeem(State(s):State<AppState>,Json(q):Json<Redeem>)->impl IntoResponse{let item=s.tickets.lock().await.remove(&q.ticket);match item{Some((expires,token)) if expires>Instant::now()=>{let mut h=HeaderMap::new();h.insert(header::CACHE_CONTROL,"no-store".parse().unwrap());(h,Json(token)).into_response()},_=>(StatusCode::GONE,Json(ErrorBody{error:"ticket invalid or expired".into()})).into_response()}}
async fn health()->&'static str{"ok"}
#[tokio::main]async fn main(){let client_id=std::env::var("BANGUMI_CLIENT_ID").expect("BANGUMI_CLIENT_ID required");let client_secret=std::env::var("BANGUMI_CLIENT_SECRET").expect("BANGUMI_CLIENT_SECRET required");let redirect_uri=std::env::var("BANGUMI_REDIRECT_URI").unwrap_or_else(|_|"http://127.0.0.1:8787/oauth/callback".into());let state=AppState{client:reqwest::Client::new(),client_id,client_secret,redirect_uri,tickets:Default::default()};let app=Router::new().route("/oauth/start",get(start)).route("/oauth/callback",get(callback)).route("/oauth/redeem",post(redeem)).route("/health",get(health)).with_state(state);let listener=tokio::net::TcpListener::bind("0.0.0.0:8787").await.unwrap();axum::serve(listener,app).await.unwrap()}
