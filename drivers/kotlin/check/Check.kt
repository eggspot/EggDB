import app.eggspot.eggdb.EggDB
import app.eggspot.eggdb.EggDBException
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
    file.delete()
    println("Kotlin over JNI and the C API: passed")
}
