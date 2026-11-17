(ns metabase.driver.eggdb
  (:require [metabase.driver :as driver]
            [metabase.driver.sql-jdbc.connection :as sql-jdbc.conn]))

(driver/register! :eggdb, :parent :postgres)

(def ^:private dialects #{"postgresql" "mysql" "sqlserver" "oracle" "sqlite"})

(defmethod driver/display-name :eggdb [_] "EggDB")

(defmethod sql-jdbc.conn/connection-details->spec :eggdb
  [_ details]
  (let [spec    (sql-jdbc.conn/connection-details->spec :postgres details)
        dialect (get dialects (:dialect details) "postgresql")]
    (cond-> spec
      (not= dialect "postgresql") (assoc :options (str "-c eggdb.dialect=" dialect)))))

(doseq [feature [:foreign-keys :nested-queries :set-timezone :window-functions/cumulative]]
  (defmethod driver/database-supports? [:eggdb feature] [_ _ _] true))
