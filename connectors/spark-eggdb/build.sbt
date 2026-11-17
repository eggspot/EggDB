name := "spark-eggdb"
organization := "app.eggspot"
version := "0.1.0"
scalaVersion := "2.12.18"
libraryDependencies ++= Seq(
  "org.apache.spark" %% "spark-sql" % "3.5.1" % Provided,
  "org.postgresql" % "postgresql" % "42.7.3"
)
