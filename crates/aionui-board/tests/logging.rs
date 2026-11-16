//! R10: every board write runs in a `board_write` span carrying `kind`,
//! `card_id`, `actor_kind`, `actor_ref`.

#[path = "common/mod.rs"]
mod common;

use std::sync::{Arc, Mutex};

use common::*;
use tracing::Subscriber;
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id};
use tracing_subscriber::layer::{Context, Layer, SubscriberExt};
use tracing_subscriber::registry::Registry;

#[derive(Default)]
struct Fields(Vec<(String, String)>);

impl Visit for Fields {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.0.push((
            field.name().to_owned(),
            format!("{value:?}").trim_matches('"').to_owned(),
        ));
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        self.0.push((field.name().to_owned(), value.to_owned()));
    }
}

/// Field `(name, value)` pairs of one span.
type SpanFields = Vec<(String, String)>;

#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<SpanFields>>>);

impl<S: Subscriber> Layer<S> for Capture {
    fn on_new_span(&self, attrs: &Attributes<'_>, _id: &Id, _ctx: Context<'_, S>) {
        if attrs.metadata().name() == "board_write" {
            let mut fields = Fields::default();
            attrs.record(&mut fields);
            self.0.lock().unwrap().push(fields.0);
        }
    }
}

#[tokio::test]
async fn writes_open_a_board_write_span_with_actor_and_card() {
    let capture = Capture::default();
    let subscriber = Registry::default().with(capture.clone());
    let _guard = tracing::subscriber::set_default(subscriber);

    let env = env().await;
    enable(&env.service, PROJECT).await;
    let card = env.service.create_card(&owner(), PROJECT, create("a")).await.unwrap();
    env.service
        .move_card(&owner(), &card.id, mv("inbox", "ideas"))
        .await
        .unwrap();

    let spans = capture.0.lock().unwrap().clone();
    let field =
        |span: &SpanFields, name: &str| span.iter().find(|(key, _)| key == name).map(|(_, value)| value.clone());
    let kinds: Vec<_> = spans.iter().filter_map(|span| field(span, "kind")).collect();
    assert_eq!(kinds, ["board_created", "card_created", "card_moved"]);
    let moved = &spans[2];
    assert_eq!(field(moved, "card_id").as_deref(), Some(card.id.as_str()));
    assert_eq!(field(moved, "actor_kind").as_deref(), Some("user"));
    assert_eq!(field(moved, "actor_ref").as_deref(), Some(USER));
}
