#![allow(dead_code, unused_imports)]

use async_channel::{unbounded, Receiver, Sender};
use async_std::net::TcpStream;
use async_std::prelude::*;
use async_std::sync::{Arc, Condvar, Mutex};
use async_std::task;
use automerge::ObjId;
use automerge::ROOT;
use futures_lite::io::{AsyncRead, AsyncWrite};
use futures_lite::stream::StreamExt;
use hypercore_protocol::{discovery_key, Channel, Event, Message, Protocol, ProtocolBuilder};
use hypercore_protocol::{schema::*, DiscoveryKey};
use hypermerge::Hypermerge;
use hypermerge::Patch;
use hypermerge::StateEvent;
use hypermerge::SynchronizeEvent;
use hypermerge::Value;
use random_access_memory::RandomAccessMemory;
use std::io;
use std::time::Duration;

mod common;
use common::*;

#[derive(Clone, Debug, Default)]
struct ProtocolThreeWritersResult {
    joiner_merge: Option<String>,
    creator_merge: Option<String>,
}
impl ProtocolThreeWritersResult {
    pub fn merge_equals(&self) -> bool {
        self.joiner_merge.as_ref().unwrap() == self.creator_merge.as_ref().unwrap()
    }
}

#[async_std::test]
async fn protocol_three_writers() -> anyhow::Result<()> {
    let (proto_responder, proto_initiator) = create_pair_memory().await?;

    let (creator_state_event_sender, mut creator_state_event_receiver): (
        Sender<StateEvent>,
        Receiver<StateEvent>,
    ) = unbounded();
    let (joiner_state_event_sender, mut joiner_state_event_receiver): (
        Sender<StateEvent>,
        Receiver<StateEvent>,
    ) = unbounded();
    let mut hypermerge_creator =
        Hypermerge::create_doc_memory("creator", vec![("version", 1)]).await;

    // Insert a map with a text field to the document
    let texts_id = hypermerge_creator
        .put_object(ROOT, "texts", automerge::ObjType::Map)
        .await
        .unwrap();
    let text_id = hypermerge_creator
        .put_object(&texts_id, "text", automerge::ObjType::Text)
        .await
        .unwrap();

    // Set watching for the prop
    hypermerge_creator
        .watch(vec![texts_id.clone(), text_id.clone()])
        .await;

    let hypermerge_creator_for_task = hypermerge_creator.clone();
    task::spawn(async move {
        connect(
            hypermerge_creator_for_task,
            proto_responder,
            creator_state_event_sender,
        )
        .await
        .unwrap();
    });

    let mut hypermerge_joiner =
        Hypermerge::register_doc_memory("joiner", &hypermerge_creator.doc_url()).await;
    let hypermerge_joiner_for_task = hypermerge_joiner.clone();
    task::spawn(async move {
        connect(
            hypermerge_joiner_for_task,
            proto_initiator,
            joiner_state_event_sender,
        )
        .await
        .unwrap();
    });

    let cork_sync_creator = Arc::new((Mutex::new(false), Condvar::new()));
    let cork_sync_joiner = Arc::clone(&cork_sync_creator);
    let mut merge_result_for_creator = Arc::new(Mutex::new(ProtocolThreeWritersResult::default()));
    let mut merge_result_for_joiner = merge_result_for_creator.clone();

    // Simulate UI threads here
    let handle = task::spawn(async move {
        let mut peers_synced = false;
        let mut texts_id: Option<ObjId> = None;
        let mut text_id: Option<ObjId> = None;
        let mut document_changes: Vec<Vec<Patch>> = vec![];
        while let Some(event) = joiner_state_event_receiver.next().await {
            println!("TEST: JOINER got event {:?}", event);
            match event {
                StateEvent::PeersSynced(len) => {
                    assert_eq!(len, 1);
                    if !peers_synced {
                        let (_value, local_texts_id) =
                            hypermerge_joiner.get(ROOT, "texts").await.unwrap().unwrap();
                        texts_id = Some(local_texts_id.clone());
                        let (_value, local_text_id) = hypermerge_joiner
                            .get(local_texts_id, "text")
                            .await
                            .unwrap()
                            .unwrap();
                        assert_text_equals(&hypermerge_joiner, &local_text_id, "").await;
                        text_id = Some(local_text_id.clone());
                        hypermerge_joiner
                            .watch(vec![texts_id.clone().unwrap(), text_id.clone().unwrap()])
                            .await;
                    }
                    peers_synced = true;
                }
                StateEvent::RemotePeerSynced() => {}
                StateEvent::DocumentChanged(patches) => {
                    println!(
                        "TEST: JOINER document_changes LEN {}",
                        document_changes.len()
                    );
                    if document_changes.len() == 0 {
                        assert_eq!(patches.len(), 5); // "Hello" has 5 chars
                        document_changes.push(patches);
                        let text_id = text_id.clone().unwrap();
                        assert_text_equals(&hypermerge_joiner, &text_id, "Hello").await;
                        hypermerge_joiner
                            .splice_text(&text_id, 5, 0, ", world!")
                            .await
                            .unwrap();
                    } else if document_changes.len() == 1 {
                        assert_eq!(patches.len(), 8); // ", world!" has 8 chars
                        document_changes.push(patches);
                        let text_id = text_id.clone().unwrap();
                        assert_text_equals(&hypermerge_joiner, &text_id, "Hello, world!").await;

                        // Let's make sure via variable that the other end is also ready to cork
                        let (lock, cvar) = &*cork_sync_joiner;
                        let mut started = lock.lock().await;
                        while !*started {
                            started = cvar.wait(started).await;
                        }

                        // Ok, ready to cork in unison
                        hypermerge_joiner.cork().await;
                        hypermerge_joiner
                            .splice_text(&text_id, 5, 2, "")
                            .await
                            .unwrap();
                        hypermerge_joiner
                            .splice_text(&text_id, 4, 0, "XX")
                            .await
                            .unwrap();
                        assert_text_equals(&hypermerge_joiner, &text_id, "HellXXoworld!").await;
                    } else if document_changes.len() == 2 {
                        // This is the two local deletions
                        assert_eq!(patches.len(), 2);
                        document_changes.push(patches);
                    } else if document_changes.len() == 3 {
                        // This is the two local additions
                        assert_eq!(patches.len(), 2);
                        document_changes.push(patches);
                    } else if document_changes.len() == 4 {
                        assert_eq!(patches.len(), 3); // One overlapping delete, and two Y
                        document_changes.push(patches);
                        let text_id = text_id.clone().unwrap();
                        merge_result_for_joiner.lock().await.joiner_merge = Some(
                            assert_text_equals_either(
                                &hypermerge_joiner,
                                &text_id,
                                "HellXXYYworld!",
                                "HellYYXXworld!",
                            )
                            .await,
                        );
                        // These are the changes sent by the peer, uncork to send the changes to the peer now
                        hypermerge_joiner.uncork().await.unwrap();
                    } else {
                        panic!("Did not expect more joiner document changes");
                    }
                }
            }
        }
    });

    let mut document_changes: Vec<Vec<Patch>> = vec![];
    let mut peers_synced = false;
    let mut remote_peer_synced = false;
    while let Some(event) = creator_state_event_receiver.next().await {
        println!("TEST: CREATOR got event {:?}", event);
        let text_id = text_id.clone();
        match event {
            StateEvent::PeersSynced(len) => {
                assert_eq!(len, 1);
                peers_synced = true;
            }
            StateEvent::RemotePeerSynced() => {
                if !remote_peer_synced {
                    hypermerge_creator
                        .splice_text(&text_id, 0, 0, "Hello")
                        .await
                        .unwrap();
                    assert_text_equals(&hypermerge_creator, &text_id, "Hello").await;
                    remote_peer_synced = true;
                }
            }
            StateEvent::DocumentChanged(patches) => {
                println!(
                    "TEST: CREATOR document_changes LEN {}",
                    document_changes.len()
                );
                if document_changes.len() == 0 {
                    assert_eq!(patches.len(), 2); // Original creation of "texts" and "text";
                    document_changes.push(patches);
                } else if document_changes.len() == 1 {
                    assert_eq!(patches.len(), 5); // "Hello" has 5 chars
                    document_changes.push(patches);
                } else if document_changes.len() == 2 {
                    assert_eq!(patches.len(), 8); // ", world!" has 8 chars
                    document_changes.push(patches);
                    assert_text_equals(&hypermerge_creator, &text_id, "Hello, world!").await;

                    // Ready to notify about cork
                    let (lock, cvar) = &*cork_sync_creator;
                    let mut started = lock.lock().await;
                    *started = true;
                    cvar.notify_one();

                    // Ok, ready to cork
                    hypermerge_creator.cork().await;

                    // Let's create a conflict here, cork to prevent sending these changes to the
                    // peer
                    hypermerge_creator
                        .splice_text(&text_id, 4, 2, "")
                        .await
                        .unwrap();
                    hypermerge_creator
                        .splice_text(&text_id, 4, 0, "YY")
                        .await
                        .unwrap();
                    assert_text_equals(&hypermerge_creator, &text_id, "HellYY world!").await;
                } else if document_changes.len() == 3 {
                    // This is the two local deletions
                    assert_eq!(patches.len(), 2);
                    document_changes.push(patches);
                } else if document_changes.len() == 4 {
                    // This is the two local additions
                    assert_eq!(patches.len(), 2);
                    document_changes.push(patches);
                    // Uncork to send the changes to the peer now
                    hypermerge_creator.uncork().await?;
                } else if document_changes.len() == 5 {
                    assert_eq!(patches.len(), 3); // One deletion that wasn't joined and two X chars
                    document_changes.push(patches);
                    let mut merge_result = merge_result_for_creator.lock().await;
                    merge_result.creator_merge = Some(
                        assert_text_equals_either(
                            &hypermerge_creator,
                            &text_id,
                            "HellXXYYworld!",
                            "HellYYXXworld!",
                        )
                        .await,
                    );
                    assert!(merge_result.merge_equals());
                } else if document_changes.len() == 6 {
                    panic!("Did not expect more creator document changes");
                }
            }
        }
    }
    Ok(())
}

async fn connect(
    mut hypermerge: Hypermerge<RandomAccessMemory>,
    mut protocol: MemoryProtocol,
    state_event_sender: Sender<StateEvent>,
) -> anyhow::Result<()> {
    let (mut sync_event_sender, mut sync_event_receiver): (
        Sender<SynchronizeEvent>,
        Receiver<SynchronizeEvent>,
    ) = unbounded();
    let mut hypermerge_for_task = hypermerge.clone();
    task::spawn(async move {
        hypermerge_for_task
            .connect_document(state_event_sender, &mut sync_event_receiver)
            .await
            .expect("Connect should not thorw error");
    });
    hypermerge
        .connect_protocol(&mut protocol, &mut sync_event_sender)
        .await?;
    Ok(())
}

async fn assert_text_equals(
    hypermerge: &Hypermerge<RandomAccessMemory>,
    obj: &ObjId,
    expected: &str,
) {
    let result = hypermerge.realize_text(obj).await;
    assert_eq!(result.unwrap().unwrap(), expected);
}

async fn assert_text_equals_either(
    hypermerge: &Hypermerge<RandomAccessMemory>,
    obj: &ObjId,
    expected_1: &str,
    expected_2: &str,
) -> String {
    let result = hypermerge.realize_text(obj).await.unwrap().unwrap();
    if result == expected_1 {
        return expected_1.to_string();
    } else if result == expected_2 {
        return expected_1.to_string();
    } else {
        panic!("Text did not match either {} or {}", expected_1, expected_2);
    }
}
