-- `admin_only` says the wrong thing twice.
--
-- The value admits **any roster role** -- a helper included -- plus a site admin, which is what
-- `may_see_spoiler` has always resolved it to. So "admin" is wrong about who it lets in, and wrong
-- about which kind of admin: this is a fact about one room's staff rather than about the site.
--
-- A catalog edit, not a table rewrite: no row is touched and the column keeps its storage, so this
-- is cheap on a table of any size.
ALTER TYPE spoiler_policy RENAME VALUE 'admin_only' TO 'staff';
