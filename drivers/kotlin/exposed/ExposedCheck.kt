import app.eggspot.eggdb.exposed.EggDbExposed
import org.jetbrains.exposed.dao.id.IntIdTable
import org.jetbrains.exposed.sql.SchemaUtils
import org.jetbrains.exposed.sql.insert
import org.jetbrains.exposed.sql.selectAll
import org.jetbrains.exposed.sql.transactions.transaction
import java.io.File

object Eggs : IntIdTable("eggs") {
    val colour = varchar("colour", 20)
}

fun main() {
    val file = File.createTempFile("eggdb-exposed", ".eggdb").apply { delete() }
    val database = EggDbExposed.connect(file.path)
    val colours = transaction(database) {
        SchemaUtils.create(Eggs)
        Eggs.insert { it[colour] = "brown" }
        Eggs.insert { it[colour] = "white" }
        Eggs.selectAll().orderBy(Eggs.id).map { it[Eggs.colour] }
    }
    check(colours == listOf("brown", "white")) { colours }
    file.delete()
    println("Exposed over EggDB's JDBC driver: passed")
}
