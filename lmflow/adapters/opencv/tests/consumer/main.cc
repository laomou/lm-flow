#include <lmflow/opencv.hpp>

int main() {
  cv::Mat image(2, 2, CV_8UC1);
  lmflow::Packet packet = lmflow::AdoptMat(std::move(image));
  return packet.IsEmpty() ? 1 : 0;
}
