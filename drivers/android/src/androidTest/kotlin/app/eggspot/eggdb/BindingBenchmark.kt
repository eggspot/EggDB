package app.eggspot.eggdb

import android.database.sqlite.SQLiteDatabase
import android.util.Log
import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import java.io.File
import org.junit.Assert.assertTrue
import org.junit.Test
import org.junit.runner.RunWith

@RunWith(AndroidJUnit4::class)
class BindingBenchmark {
    private val rows = 5_000
    private val lookups = 2_000
    private val slowestRatio = 2.0

    private fun scratch(name: String): File {
        val directory = InstrumentationRegistry.getInstrumentation().targetContext.filesDir
        return File(directory, name).apply { delete() }
    }

    private fun millis(work: () -> Unit): Double {
        val started = System.nanoTime()
        work()
        return (System.nanoTime() - started) / 1e6
    }

    private fun eggdb(): Pair<Double, Double> {
        val file = scratch("bench.eggdb")
        EggDB(file.path).use { db ->
            db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, score REAL)")
            val insert = millis {
                db.prepare("INSERT INTO t VALUES (?, ?, ?)").use { statement ->
                    db.transaction { statement.executeBatch((1..rows).map { listOf(it.toLong(), "name $it", it * 0.25) }) }
                }
            }
            val select = millis {
                db.prepare("SELECT name FROM t WHERE id = ?").use { statement ->
                    for (at in 1..lookups) statement.execute((at * 7 % rows + 1).toLong())
                }
            }
            file.delete()
            return insert to select
        }
    }

    private fun sqlite(): Pair<Double, Double> {
        val file = scratch("bench.sqlite")
        val db = SQLiteDatabase.openOrCreateDatabase(file, null)
        db.execSQL("CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, score REAL)")
        val insert = millis {
            val statement = db.compileStatement("INSERT INTO t VALUES (?, ?, ?)")
            db.beginTransaction()
            for (at in 1..rows) {
                statement.bindLong(1, at.toLong())
                statement.bindString(2, "name $at")
                statement.bindDouble(3, at * 0.25)
                statement.executeInsert()
            }
            db.setTransactionSuccessful()
            db.endTransaction()
            statement.close()
        }
        val select = millis {
            val statement = db.compileStatement("SELECT name FROM t WHERE id = ?")
            for (at in 1..lookups) {
                statement.bindLong(1, (at * 7 % rows + 1).toLong())
                statement.simpleQueryForString()
            }
            statement.close()
        }
        db.close()
        file.delete()
        return insert to select
    }

    @Test
    fun preparedStatementsAgainstAndroidsSqliteBinding() {
        eggdb()
        sqlite()
        val (eggInsert, eggSelect) = eggdb()
        val (liteInsert, liteSelect) = sqlite()
        val report = "android binding insert_ms eggdb=%.1f sqlite=%.1f select_ms eggdb=%.1f sqlite=%.1f".format(eggInsert, liteInsert, eggSelect, liteSelect)
        Log.i("EggDBBenchmark", report)
        println(report)
        assertTrue("inserts more than ${slowestRatio}x SQLite's: $report", eggInsert <= liteInsert * slowestRatio)
        assertTrue("lookups more than ${slowestRatio}x SQLite's: $report", eggSelect <= liteSelect * slowestRatio)
    }
}
