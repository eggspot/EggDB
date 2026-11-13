# Multi-primary SQL tables

Several servers can take writes to the same SQL tables, each settling conflicts by a rule chosen
per table (D1098). On every server:

```sql
CREATE EXTENSION eggdb_multi_primary;
SELECT eggdb_mp.enable('public', 'carts', 'lww', NULL, NULL);
SELECT eggdb_mp.enable('public', 'stock', 'crdt', NULL, 'on_hand,reserved');
SELECT eggdb_mp.enable('public', 'profiles', 'function', 'merge_profiles', NULL);
```

and start each server with the others:

```sh
eggdb serve a.eggdb --listen 0.0.0.0:5432 --multi-primary-node a \
  --multi-primary b=postgresql://eggdb@b.example:5432/eggdb,c=postgresql://eggdb@c.example:5432/eggdb
```

A table needs a primary key. Each local insert, update and delete is stamped with a hybrid
logical clock (milliseconds, a counter, the server's name) and kept in `eggdb_mp.log`. Each second,
every server reads the others' logs over the PostgreSQL protocol (with its own password unless the
URL's options say otherwise, and TLS as `sslmode` says), and applies their changes in one
transaction per batch, remembering in `eggdb_mp.cursors` how far it got. Applied changes are not
logged again, so they are not sent back.

| Rule | A remote change to a row also changed here |
|------|---------------------------------------------|
| `lww` | the change with the newer stamp wins, deletes included |
| `crdt` | each named counter column adds the remote change (after minus before) to the value here; the other columns follow the newer stamp |
| `function` | `resolver(local jsonb, remote jsonb, local_stamp text, remote_stamp text)` returns the row to keep as `jsonb`, or `NULL` to delete it |

A change to a row that this server has not changed is applied as it is. The rules and the stamps are
plain tables (`eggdb_mp.rules`, `eggdb_mp.stamps`) that SQL can read. Changing a row's primary
key is a delete and an insert as far as other servers are concerned; give each server's new rows
keys that cannot collide (UUIDs, or ranges per server).
