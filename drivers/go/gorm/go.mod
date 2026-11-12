module github.com/eggspot/EggDB/drivers/go/gorm

go 1.22

require (
	github.com/eggspot/EggDB/drivers/go v0.0.0
	gorm.io/driver/sqlite v1.5.6
	gorm.io/gorm v1.25.12
)

replace github.com/eggspot/EggDB/drivers/go => ../
