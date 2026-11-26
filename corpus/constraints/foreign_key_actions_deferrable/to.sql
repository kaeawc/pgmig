CREATE TABLE parent (id int PRIMARY KEY, code text CONSTRAINT parent_code_key UNIQUE);
CREATE TABLE child (
    id int,
    pid int,
    pcode text,
    -- Deferrability changes are altered in place.
    CONSTRAINT child_pid_fkey FOREIGN KEY (pid) REFERENCES parent (id) DEFERRABLE,
    CONSTRAINT child_defer_fkey FOREIGN KEY (pid) REFERENCES parent (id),
    CONSTRAINT child_late_fkey FOREIGN KEY (pid) REFERENCES parent (id) DEFERRABLE INITIALLY DEFERRED,
    -- New actions and match type: dropped and added again.
    CONSTRAINT child_pcode_fkey FOREIGN KEY (pcode) REFERENCES parent (code) MATCH FULL ON DELETE SET NULL ON UPDATE CASCADE
);
CREATE TABLE grandchild (
    cid int,
    pid int REFERENCES parent ON DELETE RESTRICT ON UPDATE SET DEFAULT DEFERRABLE INITIALLY DEFERRED,
    pcode text REFERENCES parent (code) ON DELETE NO ACTION
);
