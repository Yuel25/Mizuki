"""Build Mizuki's Windows icon assets from the selected generated master."""
import argparse
from pathlib import Path
from PIL import Image
SIZES=(16,24,32,48,64,128,256,512,1024)
def transparent(image:Image.Image)->Image.Image:
 rgb=image.convert("RGB");rgba=Image.new("RGBA",rgb.size);pixels=[]
 for r,g,b in rgb.get_flattened_data():
  chroma=max(r,g,b)-min(r,g,b);a=max(0,min(255,round((chroma-7)*255/35)));pixels.append((r,g,b,a))
 rgba.putdata(pixels);return rgba
def main():
 p=argparse.ArgumentParser();p.add_argument("source",type=Path);p.add_argument("output",type=Path);a=p.parse_args();a.output.mkdir(parents=True,exist_ok=True);master=transparent(Image.open(a.source)).resize((1024,1024),Image.Resampling.LANCZOS)
 for n in SIZES:master.resize((n,n),Image.Resampling.LANCZOS).save(a.output/f"icon-{n}.png",optimize=True)
 master.save(a.output/"icon.ico",format="ICO",sizes=tuple((n,n) for n in SIZES if n<=256))
if __name__=="__main__":main()
