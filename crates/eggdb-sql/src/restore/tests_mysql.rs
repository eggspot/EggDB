use super::classify::{Action, Effect, Ignored};
use super::classify_mysql::classify_mysql;
use super::mysql_reader::MysqlScript;
use super::script::{ScriptItem, ScriptSource};
use super::{RestoreOptions, RestoreReport, restore_dump};
use crate::dialect::Dialect;
use crate::memory_store::MemoryStore;
use crate::session::Session;
use crate::tests::render;

const DUMP: &str = r"-- MySQL dump 10.13  Distrib 8.0.36, for Linux (x86_64)
--
-- Host: localhost    Database: shop
-- ------------------------------------------------------
-- Server version	8.0.36

/*!40101 SET @OLD_CHARACTER_SET_CLIENT=@@CHARACTER_SET_CLIENT */;
/*!40101 SET @OLD_CHARACTER_SET_RESULTS=@@CHARACTER_SET_RESULTS */;
/*!50503 SET NAMES utf8mb4 */;
/*!40103 SET @OLD_TIME_ZONE=@@TIME_ZONE */;
/*!40103 SET TIME_ZONE='+00:00' */;
/*!40014 SET @OLD_UNIQUE_CHECKS=@@UNIQUE_CHECKS, UNIQUE_CHECKS=0 */;
/*!40014 SET @OLD_FOREIGN_KEY_CHECKS=@@FOREIGN_KEY_CHECKS, FOREIGN_KEY_CHECKS=0 */;
/*!40111 SET @OLD_SQL_NOTES=@@SQL_NOTES, SQL_NOTES=0 */;

--
-- Table structure for table `customers`
--

DROP TABLE IF EXISTS `customers`;
/*!40101 SET @saved_cs_client     = @@character_set_client */;
/*!50503 SET character_set_client = utf8mb4 */;
CREATE TABLE `customers` (
  `id` int NOT NULL AUTO_INCREMENT,
  `name` varchar(100) NOT NULL,
  `note` text,
  PRIMARY KEY (`id`),
  KEY `idx_name` (`name`)
) ENGINE=InnoDB AUTO_INCREMENT=4 DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci;
/*!40101 SET character_set_client = @saved_cs_client */;

--
-- Dumping data for table `customers`
--

LOCK TABLES `customers` WRITE;
/*!40000 ALTER TABLE `customers` DISABLE KEYS */;
INSERT INTO `customers` VALUES (1,'Ada','likes ; semicolons'),(2,'O\'Brien','line\nbreak'),(3,'Zoë',NULL);
/*!40000 ALTER TABLE `customers` ENABLE KEYS */;
UNLOCK TABLES;

--
-- Table structure for table `orders`
--

DROP TABLE IF EXISTS `orders`;
CREATE TABLE `orders` (
  `id` int NOT NULL AUTO_INCREMENT,
  `customer_id` int NOT NULL,
  `total` decimal(10,2) NOT NULL DEFAULT '0.00',
  PRIMARY KEY (`id`),
  KEY `customer_id` (`customer_id`),
  CONSTRAINT `orders_ibfk_1` FOREIGN KEY (`customer_id`) REFERENCES `customers` (`id`)
) ENGINE=InnoDB AUTO_INCREMENT=3 DEFAULT CHARSET=utf8mb4;

LOCK TABLES `orders` WRITE;
/*!40000 ALTER TABLE `orders` DISABLE KEYS */;
INSERT INTO `orders` VALUES (1,1,19.99),(2,2,5.00);
/*!40000 ALTER TABLE `orders` ENABLE KEYS */;
UNLOCK TABLES;
/*!40103 SET TIME_ZONE=@OLD_TIME_ZONE */;

/*!40014 SET FOREIGN_KEY_CHECKS=@OLD_FOREIGN_KEY_CHECKS */;
/*!40014 SET UNIQUE_CHECKS=@OLD_UNIQUE_CHECKS */;
/*!40101 SET CHARACTER_SET_CLIENT=@OLD_CHARACTER_SET_CLIENT */;
/*!40101 SET CHARACTER_SET_RESULTS=@OLD_CHARACTER_SET_RESULTS */;
/*!40111 SET SQL_NOTES=@OLD_SQL_NOTES */;

-- Dump completed on 2024-03-01 10:00:00
";

fn statements(script: &str) -> Vec<(String, u64)> {
    let mut input = script.as_bytes();
    let mut reader = MysqlScript::new(&mut input);
    let mut out = Vec::new();
    loop {
        match reader.next_item().unwrap() {
            ScriptItem::Statement(statement) => out.push((statement.text, statement.line)),
            ScriptItem::End => return out,
            ScriptItem::ClientCommand(_) | ScriptItem::Failed(_) => {}
        }
    }
}

fn restore(store: &MemoryStore, script: &str) -> RestoreReport {
    let mut session = Session::with_dialect(store, Dialect::MySql);
    let mut input = script.as_bytes();
    let report = restore_dump(&mut session, &mut input, RestoreOptions::default(), &mut |_| {});
    let _ = session.close();
    report
}

fn query(store: &MemoryStore, sql: &str) -> String {
    let mut session = Session::with_dialect(store, Dialect::MySql);
    let mut out = Vec::new();
    session
        .execute(sql, &mut |result| out.push(render(&result)))
        .unwrap_or_else(|error| panic!("{sql}: {error}"));
    out.join("\n")
}

#[test]
fn statements_end_at_the_delimiter_outside_strings_names_and_comments() {
    let script = "SELECT 'a;b', \"c;d\", `e;f` -- x;y\n;\n# hash; comment\nSELECT 1 /* block; */ + 1;\n/*!40101 SET NAMES utf8 */;\nDELIMITER ;;\nCREATE TRIGGER t BEFORE INSERT ON x FOR EACH ROW BEGIN SET NEW.a = 1; END ;;\ndelimiter ;\nSELECT 'it''s', 'back\\'slash';";
    let found = statements(script);
    let texts: Vec<&str> = found.iter().map(|(text, _)| text.as_str()).collect();
    assert_eq!(
        texts,
        [
            "SELECT 'a;b', \"c;d\", `e;f`",
            "SELECT 1   + 1",
            "/*!40101 SET NAMES utf8 */",
            "CREATE TRIGGER t BEFORE INSERT ON x FOR EACH ROW BEGIN SET NEW.a = 1; END",
            "SELECT 'it''s', 'back\\'slash'",
        ]
    );
    assert_eq!(found.iter().map(|(_, line)| *line).collect::<Vec<_>>(), [1, 4, 5, 7, 9]);
    let mut unclosed = "SELECT 'open;".as_bytes();
    let mut reader = MysqlScript::new(&mut unclosed);
    assert!(reader.next_item().is_err());
}

#[test]
fn the_classifier_reports_what_each_dump_statement_does() {
    let none = |_: &str| false;
    assert_eq!(classify_mysql("CREATE TABLE `shop`.`t` (a int)", &none), Action::Run(Effect::Table { name: "shop.t".to_owned(), constraints: 0 }));
    assert_eq!(classify_mysql("INSERT IGNORE INTO `t` VALUES (1)", &none), Action::Run(Effect::Insert("t".to_owned())));
    assert_eq!(classify_mysql("/*!40000 ALTER TABLE `t` DISABLE KEYS */", &none), Action::Ignore(Ignored::Other("ALTER TABLE ... DISABLE KEYS and ENABLE KEYS")));
    assert!(matches!(classify_mysql("LOCK TABLES `t` WRITE", &none), Action::Ignore(_)));
    assert!(matches!(classify_mysql("CREATE DATABASE /*!32312 IF NOT EXISTS*/ `shop`", &none), Action::Ignore(_)));
    assert!(matches!(classify_mysql("USE `shop`", &none), Action::Ignore(_)));
    assert_eq!(classify_mysql("SET @@GLOBAL.GTID_PURGED='x'", &none), Action::Ignore(Ignored::Set("gtid_purged".to_owned())));
    assert_eq!(classify_mysql("SET @@SESSION.SQL_LOG_BIN= 0", &none), Action::Ignore(Ignored::Set("sql_log_bin".to_owned())));
    assert_eq!(classify_mysql("/*!40101 SET NAMES utf8mb4 */", &none), Action::Run(Effect::Setting));
    assert_eq!(
        classify_mysql("/*!50001 CREATE ALGORITHM=UNDEFINED */ /*!50013 DEFINER=`root`@`localhost` SQL SECURITY DEFINER */ /*!50001 VIEW `v` AS select 1 */", &none),
        Action::Run(Effect::View("v".to_owned()))
    );
    assert_eq!(
        classify_mysql("/*!50003 CREATE*/ /*!50017 DEFINER=`root`@`%`*/ /*!50003 TRIGGER `stamp` BEFORE INSERT ON `t` FOR EACH ROW SET NEW.a = 1 */", &none),
        Action::Run(Effect::Routine { kind: "trigger", name: "stamp".to_owned() })
    );
}

#[test]
fn a_mysqldump_file_restores_tables_rows_and_keys() {
    let store = MemoryStore::default();
    let report = restore(&store, DUMP);
    assert!(report.error.is_none(), "{:?}", report.error);
    assert_eq!((report.tables, report.rows), (2, 5));
    assert!(report.ignored.other.iter().any(|(reason, _)| reason.contains("LOCK TABLES")), "{:?}", report.ignored);
    assert_eq!(
        query(&store, "SELECT id, name, note FROM customers ORDER BY id"),
        query(&store, "SELECT 1 AS id, 'Ada' AS name, 'likes ; semicolons' AS note UNION ALL SELECT 2, 'O''Brien', 'line\nbreak' UNION ALL SELECT 3, 'Zoë', NULL")
    );
    assert_eq!(query(&store, "SELECT SUM(total) FROM orders"), query(&store, "SELECT CAST(24.99 AS DECIMAL(10,2))"));
    query(&store, "INSERT INTO customers (name) VALUES ('Grace')");
    assert_eq!(query(&store, "SELECT MAX(id) FROM customers"), query(&store, "SELECT 4"));
}
