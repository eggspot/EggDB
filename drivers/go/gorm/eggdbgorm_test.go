package eggdbgorm

import (
	"testing"

	"gorm.io/gorm"
)

type Egg struct {
	ID     uint
	Colour string
	Weight int
}

func TestModelsMigrateCreateQueryUpdateAndDelete(t *testing.T) {
	db, err := gorm.Open(Open(":memory:"), &gorm.Config{})
	if err != nil {
		t.Fatal(err)
	}
	if err := db.AutoMigrate(&Egg{}); err != nil {
		t.Fatal(err)
	}
	eggs := []Egg{{Colour: "brown", Weight: 61}, {Colour: "white", Weight: 58}}
	if err := db.Create(&eggs).Error; err != nil {
		t.Fatal(err)
	}
	if eggs[0].ID == 0 || eggs[1].ID == 0 {
		t.Fatalf("the created rows have no ids: %+v", eggs)
	}
	var found []Egg
	if err := db.Order("id").Find(&found).Error; err != nil {
		t.Fatal(err)
	}
	if len(found) != 2 || found[1].Colour != "white" {
		t.Fatalf("found %+v", found)
	}
	if err := db.Model(&Egg{}).Where("colour = ?", "white").Update("weight", 59).Error; err != nil {
		t.Fatal(err)
	}
	var white Egg
	if err := db.First(&white, "colour = ?", "white").Error; err != nil || white.Weight != 59 {
		t.Fatalf("white egg %+v, %v", white, err)
	}
	if err := db.Delete(&Egg{}, eggs[0].ID).Error; err != nil {
		t.Fatal(err)
	}
	var count int64
	db.Model(&Egg{}).Count(&count)
	if count != 1 {
		t.Fatalf("%d eggs left", count)
	}
}
