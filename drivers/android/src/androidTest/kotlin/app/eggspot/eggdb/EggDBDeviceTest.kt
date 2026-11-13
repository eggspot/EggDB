package app.eggspot.eggdb

import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import java.io.File
import org.junit.Assert.assertEquals
import org.junit.Test
import org.junit.runner.RunWith

@RunWith(AndroidJUnit4::class)
class EggDBDeviceTest {
    private fun scratch(name: String): File {
        val directory = InstrumentationRegistry.getInstrumentation().targetContext.filesDir
        return File(directory, name).apply { delete() }
    }

    @Test
    fun sqlRunsOnTheDevice() {
        val file = scratch("device.eggdb")
        EggDB(file.path).use { db ->
            db.execute("CREATE TABLE eggs (id INTEGER PRIMARY KEY, name TEXT)")
            db.execute("INSERT INTO eggs VALUES (1, 'brown 🥚')")
            val rows = db.execute("SELECT name FROM eggs").single().rows
            assertEquals(listOf(listOf<Value>(Value.Text("brown 🥚"))), rows)
        }
        file.delete()
    }

    @Test
    fun keyValueSurvivesSuspendAndReopen() {
        val file = scratch("device-kv.eggdb")
        KeyValue(file.path).use { kv ->
            kv.write { txn -> txn.put("battery", "low") }
            kv.setPowerSource(PowerSource.LOW_BATTERY)
            kv.suspend()
            kv.resume()
            kv.write { txn -> txn.put("after", "resume") }
        }
        KeyValue(file.path, readOnly = true).use { kv ->
            kv.read { txn ->
                assertEquals("low", txn.getText("battery"))
                assertEquals("resume", txn.getText("after"))
            }
        }
        file.delete()
    }
}
