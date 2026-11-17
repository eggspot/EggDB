# Power BI connector for EggDB

A Power Query custom connector (`EggDB.pq`) that shows **EggDB** in Power BI Desktop's Get Data
list and reads an EggDB server's tables and views over its PostgreSQL protocol, with a user name
and password and encryption on.

Build `EggDB.mez` with the Power Query SDK (Visual Studio Code's Power Query SDK extension:
"Build" on this folder, with the eight icon sizes `EggDB16.png` to `EggDB64.png` beside it) and
put it in `Documents\Power BI Desktop\Custom Connectors`; in Power BI Desktop allow custom
connectors under Options → Security → Data Extensions. On the on-premises data gateway, put it in
the gateway's custom connectors folder.

Licensed under MIT OR Apache-2.0, like EggDB.
