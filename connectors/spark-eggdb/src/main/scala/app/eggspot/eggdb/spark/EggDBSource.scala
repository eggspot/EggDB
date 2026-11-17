package app.eggspot.eggdb.spark

import org.apache.spark.sql.{DataFrame, SQLContext, SaveMode}
import org.apache.spark.sql.execution.datasources.jdbc.JdbcRelationProvider
import org.apache.spark.sql.sources.{BaseRelation, DataSourceRegister}

class EggDBSource extends JdbcRelationProvider with DataSourceRegister {
  override def shortName(): String = "eggdb"

  override def createRelation(sqlContext: SQLContext, parameters: Map[String, String]): BaseRelation =
    super.createRelation(sqlContext, EggDBSource.jdbcOptions(parameters))

  override def createRelation(
      sqlContext: SQLContext,
      mode: SaveMode,
      parameters: Map[String, String],
      data: DataFrame): BaseRelation =
    super.createRelation(sqlContext, mode, EggDBSource.jdbcOptions(parameters), data)
}

object EggDBSource {
  val Dialects: Set[String] = Set("postgresql", "mysql", "sqlserver", "oracle", "sqlite")

  def url(parameters: Map[String, String]): String = {
    val host = parameters.getOrElse("host", "localhost")
    val port = parameters.getOrElse("port", "5432")
    val database = parameters.getOrElse("database", "eggdb")
    val dialect = parameters.getOrElse("dialect", "postgresql")
    require(Dialects.contains(dialect), s"EggDB dialect $dialect is not one of ${Dialects.mkString(", ")}")
    val options = if (dialect == "postgresql") "" else s"&options=-c%20eggdb.dialect%3D$dialect"
    val ssl = parameters.getOrElse("sslmode", "prefer")
    s"jdbc:postgresql://$host:$port/$database?sslmode=$ssl$options"
  }

  def jdbcOptions(parameters: Map[String, String]): Map[String, String] = {
    val withUrl = if (parameters.contains("url")) parameters else parameters + ("url" -> url(parameters))
    withUrl + ("driver" -> "org.postgresql.Driver")
  }
}
