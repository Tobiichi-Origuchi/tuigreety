use std::time::Duration;

use chrono::Local;
use libgreetd_stub::SessionOptions;

use super::common::IntegrationRunner;

#[tokio::test]
async fn show_greet() {
  let opts = SessionOptions {
    username: "apognu".to_string(),
    password: "password".to_string(),
    mfa: false,
  };

  let mut runner = IntegrationRunner::new(
    opts,
    Some(|greeter| {
      greeter.greeting = Some("Lorem ipsum dolor sit amet".to_string());
    }),
  )
  .await;

  let events = tokio::task::spawn({
    let mut runner = runner.clone();

    async move {
      runner.wait_for_render().await;

      assert!(runner.output().await.contains("Lorem ipsum dolor sit amet"));
    }
  });

  runner.join_until_end(events).await;
}

#[tokio::test]
async fn show_wrapped_greet() {
  let opts = SessionOptions {
    username: "apognu".to_string(),
    password: "password".to_string(),
    mfa: false,
  };

  let mut runner = IntegrationRunner::new_with_size(
    opts,
    Some(|greeter| {
      greeter.greeting = Some("Lorem \x1b[31mipsum dolor sit amet".to_string());
    }),
    (20, 20),
  )
  .await;

  let events = tokio::task::spawn({
    let mut runner = runner.clone();

    async move {
      runner.wait_for_render().await;

      let output = runner.output().await;

      assert!(output.contains("┌ Authenticate into┐"));
      assert!(output.contains("│    Lorem ipsum   │"));
      assert!(output.contains("│  dolor sit amet  │"));
      assert!(output.contains("└──────────────────┘"));
    }
  });

  runner.join_until_end(events).await;
}

const TIME_FORMAT: &str = "%Y-%m-%dT%H:%M:%S";

#[tokio::test]
async fn show_time() {
  let opts = SessionOptions {
    username: "apognu".to_string(),
    password: "password".to_string(),
    mfa: false,
  };

  let time_before_start = Local::now().format(TIME_FORMAT).to_string();

  let mut runner = IntegrationRunner::new(
    opts,
    Some(|greeter| {
      greeter.time = true;
      greeter.time_format = Some(TIME_FORMAT.to_string());
    }),
  )
  .await;

  let events = tokio::task::spawn({
    let mut runner = runner.clone();

    async move {
      runner.wait_for_render().await;

      let output = runner.output().await;
      let time_after_render = Local::now().format(TIME_FORMAT).to_string();
      let rendered_time = if output.contains(&time_before_start) {
        time_before_start
      } else {
        assert!(output.contains(&time_after_render));
        time_after_render
      };

      tokio::time::sleep(Duration::from_secs(1)).await;

      runner.wait_for_render().await;

      assert!(!runner.output().await.contains(&rendered_time));
    }
  });

  runner.join_until_end(events).await;
}
