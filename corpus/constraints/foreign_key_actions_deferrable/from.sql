CREATE TABLE parent (id int PRIMARY KEY, code text CONSTRAINT parent_code_key UNIQUE);
CREATE TABLE child (
    id int,
    pid int,
    pcode text,
    CONSTRAINT child_pid_fkey FOREIGN KEY (pid) REFERENCES parent (id),
    CONSTRAINT child_pcode_fkey FOREIGN KEY (pcode) REFERENCES parent (code) ON DELETE CASCADE,
    CONSTRAINT child_defer_fkey FOREIGN KEY (pid) REFERENCES parent (id) DEFERRABLE INITIALLY DEFERRED,
    CONSTRAINT child_late_fkey FOREIGN KEY (pid) REFERENCES parent (id) DEFERRABLE
);
