-- parent goes: the kept table's foreign key to it first, the dropped child's with the child.
-- m1 and m2 reference each other, so their foreign keys are dropped before either table.
CREATE TABLE kept (id int, pid int);
