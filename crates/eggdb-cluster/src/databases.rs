//! Rebalancing databases per tenant across servers (D1048): each server holds whole database
//! files, and the plan moves whole databases from the fullest server to the emptiest while a
//! move narrows the gap between them.

use std::collections::BTreeMap;

/// One planned move: the database, the server it leaves and the server it goes to.
pub type DatabaseMove = (String, String, String);

/// The moves that even out the bytes each server holds: repeatedly the largest database of
/// the fullest server that is smaller than its gap to the emptiest goes there, so every move
/// narrows the gap and the plan ends. `servers` maps each server to its databases with their
/// sizes in bytes.
pub fn plan_database_moves(servers: &BTreeMap<String, Vec<(String, u64)>>) -> Vec<DatabaseMove> {
    let mut held: BTreeMap<String, Vec<(String, u64)>> = servers.clone();
    let total = |databases: &Vec<(String, u64)>| databases.iter().map(|(_, size)| *size).sum::<u64>();
    let mut moves = Vec::new();
    let limit = servers.values().map(Vec::len).sum::<usize>() + 1;
    for _ in 0..limit {
        let Some((fullest, fullest_bytes)) = held.iter().map(|(server, databases)| (server.clone(), total(databases))).max_by_key(|(server, bytes)| (*bytes, std::cmp::Reverse(server.clone()))) else {
            break;
        };
        let Some((emptiest, emptiest_bytes)) = held.iter().map(|(server, databases)| (server.clone(), total(databases))).min_by_key(|(server, bytes)| (*bytes, server.clone())) else {
            break;
        };
        let gap = fullest_bytes - emptiest_bytes;
        let Some(position) = held[&fullest]
            .iter()
            .enumerate()
            .filter(|(_, (_, size))| *size > 0 && *size < gap)
            .max_by_key(|(_, (name, size))| (*size, std::cmp::Reverse(name.clone())))
            .map(|(position, _)| position)
        else {
            break;
        };
        let database = held.get_mut(&fullest).map(|databases| databases.remove(position));
        let Some(database) = database else {
            break;
        };
        moves.push((database.0.clone(), fullest.clone(), emptiest.clone()));
        held.entry(emptiest).or_default().push(database);
    }
    moves
}
