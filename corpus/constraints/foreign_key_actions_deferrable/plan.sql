CREATE TABLE public.grandchild (
    cid integer,
    pid integer,
    pcode text
);
ALTER TABLE public.child DROP CONSTRAINT child_pcode_fkey;
ALTER TABLE public.grandchild ADD CONSTRAINT grandchild_pcode_fkey FOREIGN KEY (pcode) REFERENCES public.parent(code);
ALTER TABLE public.grandchild ADD CONSTRAINT grandchild_pid_fkey FOREIGN KEY (pid) REFERENCES public.parent(id) ON UPDATE SET DEFAULT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;
ALTER TABLE public.child ALTER CONSTRAINT child_defer_fkey NOT DEFERRABLE;
ALTER TABLE public.child ALTER CONSTRAINT child_late_fkey DEFERRABLE INITIALLY DEFERRED;
ALTER TABLE public.child ADD CONSTRAINT child_pcode_fkey FOREIGN KEY (pcode) REFERENCES public.parent(code) MATCH FULL ON UPDATE CASCADE ON DELETE SET NULL;
ALTER TABLE public.child ALTER CONSTRAINT child_pid_fkey DEFERRABLE INITIALLY IMMEDIATE;
