"""Minimal PE reader: exports of a DLL and the functions a module imports from one DLL.

    pe.py imports <module>...   (winmm.dll imports)
    pe.py exports <dll>
"""
import sys, struct
def load(p):
    b=open(p,'rb').read(); pe=struct.unpack_from('<I',b,0x3C)[0]
    n=struct.unpack_from('<H',b,pe+6)[0]; opt=pe+24; magic=struct.unpack_from('<H',b,opt)[0]
    dd=opt+(96 if magic==0x10b else 112)
    secs=[struct.unpack_from('<IIII',b,opt+struct.unpack_from('<H',b,pe+20)[0]+i*40+8) for i in range(n)]
    def r2o(rva):
        for vs,va,rs,ra in secs:
            if va<=rva<va+max(vs,rs): return rva-va+ra
    return b,dd,r2o,magic
def cstr(b,o): return b[o:b.index(b'\0',o)].decode()
def imports(p,dll='winmm.dll'):
    b,dd,r2o,magic=load(p); rva=struct.unpack_from('<I',b,dd+8)[0]; out=set()
    if not rva: return out
    o=r2o(rva)
    while True:
        oft,_,_,name,ft=struct.unpack_from('<IIIII',b,o)
        if not name: break
        if cstr(b,r2o(name)).lower()==dll:
            t=r2o(oft or ft)
            while True:
                v=struct.unpack_from('<I',b,t)[0]
                if not v: break
                out.add(('#%d'%(v&0xffff)) if v&0x80000000 else cstr(b,r2o(v)+2)); t+=4
        o+=20
    return out
def exports(p):
    b,dd,r2o,magic=load(p); rva=struct.unpack_from('<I',b,dd)[0]; o=r2o(rva)
    nn=struct.unpack_from('<I',b,o+24)[0]; names=r2o(struct.unpack_from('<I',b,o+32)[0])
    return [cstr(b,r2o(struct.unpack_from('<I',b,names+4*i)[0])) for i in range(nn)]
if __name__=='__main__':
    if sys.argv[1]=='imports':
        for p in sys.argv[2:]:
            s=imports(p)
            if s: print(p.split('/')[-1]+':', ' '.join(sorted(s)))
    else:
        print('\n'.join(exports(sys.argv[2])))
