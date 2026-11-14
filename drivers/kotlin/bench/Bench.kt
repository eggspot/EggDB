import app.eggspot.eggdb.EggDB
import java.io.File
import java.sql.DriverManager

private val rows = System.getenv("DRIVER_BENCH_ROWS")?.toIntOrNull() ?: 20000
private val lookups = System.getenv("DRIVER_BENCH_LOOKUPS")?.toIntOrNull() ?: 5000

private fun timed(work: () -> Unit): Double {
    val started = System.nanoTime()
    work()
    return (System.nanoTime() - started) / 1e6
}

private fun eggdbSide(path: String): List<Double> = EggDB(path).use { db ->
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, score REAL)")
    val insert = timed {
        db.prepare("INSERT INTO t VALUES (?, ?, ?)").use { statement ->
            db.transaction { statement.executeBatch((1..rows).map { listOf(it.toLong(), "name $it", it * 0.25) }) }
        }
    }
    val lookup = timed {
        db.prepare("SELECT name FROM t WHERE id = ?").use { statement ->
            for (at in 1..lookups) statement.rows((at * 7L % rows) + 1).count()
        }
    }
    val scan = timed {
        var total = 0L
        db.rows("SELECT name FROM t").forEach { total += it[0].toString().length }
    }
    listOf(insert, lookup, scan)
}

private fun sqliteSide(path: String): List<Double> = DriverManager.getConnection("jdbc:sqlite:$path").use { connection ->
    connection.createStatement().use { it.executeUpdate("CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, score REAL)") }
    val insert = timed {
        connection.autoCommit = false
        connection.prepareStatement("INSERT INTO t VALUES (?, ?, ?)").use { statement ->
            for (at in 1..rows) {
                statement.setLong(1, at.toLong())
                statement.setString(2, "name $at")
                statement.setDouble(3, at * 0.25)
                statement.addBatch()
            }
            statement.executeBatch()
        }
        connection.commit()
        connection.autoCommit = true
    }
    val lookup = timed {
        connection.prepareStatement("SELECT name FROM t WHERE id = ?").use { statement ->
            for (at in 1..lookups) {
                statement.setLong(1, (at * 7L % rows) + 1)
                statement.executeQuery().use { found -> while (found.next()) found.getString(1) }
            }
        }
    }
    val scan = timed {
        var total = 0L
        connection.prepareStatement("SELECT name FROM t").use { statement ->
            statement.executeQuery().use { found -> while (found.next()) total += found.getString(1).length }
        }
    }
    listOf(insert, lookup, scan)
}

fun main() {
    val directory = kotlin.io.path.createTempDirectory("eggdb-kotlin-bench").toFile()
    val egg = eggdbSide(File(directory, "bench.eggdb").path)
    val lite = sqliteSide(File(directory, "bench.sqlite").path)
    listOf("insert", "lookup", "scan").forEachIndexed { at, workload ->
        println("kotlin %s %.2f %.2f".format(workload, egg[at], lite[at]))
    }
    directory.deleteRecursively()
}
