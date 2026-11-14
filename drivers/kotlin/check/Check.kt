import app.eggspot.eggdb.EggDB
import app.eggspot.eggdb.EggDBException
import app.eggspot.eggdb.KeyValue
import app.eggspot.eggdb.Value
import java.io.File

fun main() {
    val file = File.createTempFile("eggdb-kotlin", ".eggdb").apply { delete() }
    EggDB(file.path).use { db ->
        db.execute("CREATE TABLE eggs (id INTEGER PRIMARY KEY, name TEXT, weight REAL, shell BLOB)")
        db.execute("INSERT INTO eggs VALUES (1, 'brown 🥚', 61.5, x'00ff'), (2, NULL, NULL, NULL)")
        val result = db.execute("SELECT id, name, weight, shell FROM eggs ORDER BY id").single()
        check(result.columns == listOf("id", "name", "weight", "shell")) { result.columns }
        check(result.rows[0] == listOf(Value.Integer(1), Value.Text("brown 🥚"), Value.Real(61.5), Value.Blob(byteArrayOf(0, -1)))) { result.rows[0] }
        check(result.rows[1] == listOf(Value.Integer(2), Value.Null, Value.Null, Value.Null)) { result.rows[1] }
        val refused = runCatching { db.execute("SELECT * FROM nowhere") }.exceptionOrNull()
        check(refused is EggDBException) { "expected an EggDBException, got $refused" }
    }
    EggDB(file.path, "postgresql").use { db ->
        check(db.execute("SELECT count(*) FROM eggs").single().rows[0][0] == Value.Integer(2))
    }
    EggDB(file.path).use { db ->
        db.prepare("INSERT INTO eggs (id, name, weight, shell) VALUES (?, ?, ?, ?)").use { insert ->
            db.transaction {
                insert.executeBatch((3L..102L).map { listOf(it, "egg $it", it * 0.5, byteArrayOf(it.toByte())) })
            }
            val refused = runCatching { db.transaction { insert.execute(3L, "again", null, null) } }.exceptionOrNull()
            check(refused is EggDBException) { "a duplicate key must fail, got $refused" }
        }
        db.prepare("SELECT name, weight FROM eggs WHERE id = ?").use { select ->
            val row = select.execute(42L).single().rows.single()
            check(row == listOf(Value.Text("egg 42"), Value.Real(21.0))) { row }
            check(select.execute(Value.Integer(1)).single().rows.single()[0] == Value.Text("brown 🥚"))
        }
        val heavy = db.rows("SELECT id FROM eggs WHERE weight > ? ORDER BY id", 50.0).map { it[0] }.toList()
        check(heavy == listOf(Value.Integer(1), Value.Integer(101), Value.Integer(102))) { heavy }
        db.prepare("SELECT name FROM eggs WHERE id < ? ORDER BY id").use { select ->
            check(select.columns() == listOf("name")) { select.columns() }
            check(select.rows(3L).count() == 2)
            check(select.rows(4L).first() == listOf(Value.Text("brown 🥚")))
        }
        db.beginTransaction()
        db.execute("DELETE FROM eggs")
        db.rollback()
        check(db.execute("SELECT count(*) FROM eggs").single().rows[0][0] == Value.Integer(102))
    }
    file.delete()
    EggDB(":memory:", "postgresql").use { db ->
        db.execute("CREATE TABLE t (id integer, name text)")
        check(db.copyFrom("COPY t FROM STDIN", listOf("1\tad".toByteArray(), "a\n2\tgrace\n".toByteArray())) == 2L)
        check(String(db.copyTo("COPY t TO STDOUT")) == "1\tada\n2\tgrace\n")
    }
    val store = File.createTempFile("eggdb-kotlin-kv", ".eggdb").apply { delete() }
    KeyValue(store.path).use { kv ->
        kv.write { txn ->
            txn.put("a", "1")
            txn.put("b".toByteArray(), byteArrayOf(0, 2))
            check(txn.createCollection("users"))
            txn.put("u1", "Ada", collection = "users")
        }
        kv.read { txn ->
            check(txn.getText("a") == "1")
            check(txn.get("missing".toByteArray()) == null)
            val all = txn.scan()
            check(all.size == 2 && all[1].value.contentEquals(byteArrayOf(0, 2))) { all.size }
            check(txn.scan(reverse = true, limit = 1).single().key.contentEquals("b".toByteArray()))
            check(txn.collections() == listOf("users")) { txn.collections() }
            check(txn.collectionStats("users").entries == 1L)
        }
        kv.write { txn ->
            check(txn.delete("a".toByteArray()))
            txn.renameCollection("users", "people")
        }
        kv.read { txn ->
            check(txn.getText("a") == null)
            check(txn.getText("u1", collection = "people") == "Ada")
        }
    }
    store.delete()
    println("Kotlin over JNI and the C API: passed")
}
