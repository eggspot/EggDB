using System;
using Microsoft.EntityFrameworkCore.Infrastructure;
using Microsoft.EntityFrameworkCore.Migrations;

[DbContext(typeof(ShopContext))]
[Migration("20261029000002_AddPosts")]
public sealed class AddPosts : Migration
{
    protected override void Up(MigrationBuilder migrationBuilder)
    {
        migrationBuilder.AddColumn<string>(
            name: "Url",
            table: "Blogs",
            type: "nvarchar(max)",
            nullable: true);

        migrationBuilder.CreateTable(
            name: "Posts",
            columns: table => new
            {
                Id = table.Column<int>(type: "int", nullable: false)
                    .Annotation("SqlServer:Identity", "1, 1"),
                Title = table.Column<string>(type: "nvarchar(200)", maxLength: 200, nullable: false),
                Rating = table.Column<int>(type: "int", nullable: false),
                Published = table.Column<DateTime>(type: "datetime2", nullable: false),
                Price = table.Column<decimal>(type: "decimal(10,2)", precision: 10, scale: 2, nullable: false),
                BlogId = table.Column<int>(type: "int", nullable: false),
            },
            constraints: table =>
            {
                table.PrimaryKey("PK_Posts", x => x.Id);
                table.ForeignKey(
                    name: "FK_Posts_Blogs_BlogId",
                    column: x => x.BlogId,
                    principalTable: "Blogs",
                    principalColumn: "Id",
                    onDelete: ReferentialAction.Cascade);
            });

        migrationBuilder.CreateIndex(
            name: "IX_Posts_BlogId",
            table: "Posts",
            column: "BlogId");
    }

    protected override void Down(MigrationBuilder migrationBuilder)
    {
        migrationBuilder.DropTable(name: "Posts");
        migrationBuilder.DropColumn(name: "Url", table: "Blogs");
    }
}
