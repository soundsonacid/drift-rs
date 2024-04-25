use crate::dlob::dlob_node::{get_order_signature, DLOBNode, Node, NodeToFill};
use std::collections::HashMap;

pub fn determine_maker_and_taker(
    ask: &Node,
    bid: &Node,
    // first node maker, second node taker
) -> Option<(Node, Node)> {
    let bid = bid.clone();
    let ask = ask.clone();
    let ask_order = ask.get_order();
    let bid_order = bid.get_order();
    let ask_slot = ask_order.slot + ask_order.auction_duration as u64;
    let bid_slot = bid_order.slot + bid_order.auction_duration as u64;

    if bid_order.post_only && ask_order.post_only {
        None
    } else if bid_order.post_only {
        Some((ask, bid))
    } else if ask_order.post_only {
        Some((bid, ask))
    } else if ask_slot <= bid_slot {
        Some((bid, ask))
    } else {
        Some((ask, bid))
    }
}

pub fn merge_and_deduplicate_nodes(
    nodes_a: Vec<NodeToFill>,
    nodes_b: Vec<NodeToFill>,
) -> Vec<NodeToFill> {
    let mut merged_nodes_map: HashMap<String, NodeToFill> = HashMap::new();
    let all_nodes = vec![nodes_a, nodes_b];

    for node_list in all_nodes.iter() {
        node_list.iter().for_each(|node| {
            let sig =
                get_order_signature(node.node.get_order().order_id, node.node.get_user_account());

            if !merged_nodes_map.contains_key(&sig) {
                merged_nodes_map.insert(
                    sig.clone(),
                    NodeToFill {
                        node: node.node.clone(),
                        maker_nodes: vec![],
                    },
                );
            }

            if node.maker_nodes.len() > 0 {
                let merged_node = merged_nodes_map.get_mut(&sig).unwrap();
                merged_node.maker_nodes.extend(node.maker_nodes.clone());
            }
        })
    }

    merged_nodes_map.values().map(|node| node.clone()).collect()
}
