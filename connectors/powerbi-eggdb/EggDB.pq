[Version = "0.1.0"]
section EggDB;

[DataSource.Kind = "EggDB", Publish = "EggDB.Publish"]
shared EggDB.Database = Value.ReplaceType(EggDBImpl, EggDBType);

EggDBType = type function (
    server as (type text meta [
        Documentation.FieldCaption = "Server",
        Documentation.SampleValues = {"localhost:5432"}
    ]),
    database as (type text meta [
        Documentation.FieldCaption = "Database",
        Documentation.SampleValues = {"eggdb"}
    ])
) as table meta [
    Documentation.Name = "EggDB",
    Documentation.LongDescription = "Reads tables and views of an EggDB server over its PostgreSQL protocol."
];

EggDBImpl = (server as text, database as text) as table =>
    PostgreSQL.Database(server, database, [CreateNavigationProperties = true]);

EggDB = [
    TestConnection = (dataSourcePath) =>
        let
            json = Json.Document(dataSourcePath),
            server = json[server],
            database = json[database]
        in
            {"EggDB.Database", server, database},
    Authentication = [
        UsernamePassword = []
    ],
    SupportsEncryption = true,
    Label = "EggDB"
];

EggDB.Publish = [
    Beta = true,
    Category = "Database",
    ButtonText = {"EggDB", "Connect to an EggDB server"},
    SourceImage = EggDB.Icons,
    SourceTypeImage = EggDB.Icons
];

EggDB.Icons = [
    Icon16 = {Extension.Contents("EggDB16.png"), Extension.Contents("EggDB20.png"), Extension.Contents("EggDB24.png"), Extension.Contents("EggDB32.png")},
    Icon32 = {Extension.Contents("EggDB32.png"), Extension.Contents("EggDB40.png"), Extension.Contents("EggDB48.png"), Extension.Contents("EggDB64.png")}
];
